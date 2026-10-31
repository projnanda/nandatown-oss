import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { afterEach, beforeEach, describe, mock, test } from "node:test";

import { InMemoryTaskStore } from "@a2a-js/sdk/server";
import { Task, TaskState } from "@a2a-js/sdk";

import {
  NasikoStateClient,
  NasikoStateConflict,
  NasikoStateMalformedRow,
  NasikoStateTooLarge,
  NasikoTaskStore,
  PENDING_MAX_AGE_MS,
  closeOpenTaskStores,
  encodeStoredTask,
  installShutdownHooks,
  taskStoreFromEnv,
} from "../src/index.js";
import { FakeStateServer, TOKEN } from "./fake-state-server.js";
import { callContext, newTask, userMessage } from "./helpers.js";

const fixture = (name) => JSON.parse(readFileSync(new URL(`./fixtures/${name}`, import.meta.url), "utf8"));

let server;
let store;
let logs;
const logger = { warn: (m) => logs.push(["warn", m]), error: (m) => logs.push(["error", m]) };

beforeEach(async () => {
  logs = [];
  server = await new FakeStateServer().start();
  store = newStore();
});
afterEach(async () => {
  mock.timers.reset();
  await store.close().catch(() => {});
  await server.stop();
});

function newStore() {
  return new NasikoTaskStore(new NasikoStateClient(server.url, TOKEN, { retryBackoffMs: 0, fetch: server.fetch }), { logger });
}

/**
 * Waits (in real time, which mock timers do not fake) until `until` holds, so in-flight HTTP can
 * finish. Fails the test when it never does.
 */
async function settle(until, timeoutMs = 3000) {
  const deadline = performance.now() + timeoutMs;
  while (!until()) {
    assert.ok(performance.now() < deadline, "timed out waiting for the store");
    await new Promise((resolve) => setImmediate(resolve));
  }
}

const storedState = (id) => server.tasks.get(id)?.state;
const writesOf = (id) => server.writes("tasks").filter((r) => r.path === `/tasks/${id}`);

describe("writes", () => {
  test("a state change is on the server before save resolves", async () => {
    await store.save(newTask("t1", "working"), callContext);
    assert.equal(storedState("t1"), "working");

    await store.save(newTask("t1", "input-required"), callContext);
    assert.equal(storedState("t1"), "input-required");

    await store.save(newTask("t1", "completed"), callContext);
    assert.equal(storedState("t1"), "completed");
    assert.equal(writesOf("t1").length, 3);
  });

  test("the stored row carries the format tag and the task id", async () => {
    await store.save(newTask("t1", "working", { contextId: "c9" }), callContext);

    const row = server.tasks.get("t1");
    assert.equal(row.context_id, "c9");
    assert.equal(row.body.format, "a2a-1");
    assert.equal(row.body.task.id, "t1");
    assert.equal(row.body.task.status.state, "TASK_STATE_WORKING");
  });

  test("saves that do not change state coalesce to one write per window", async () => {
    mock.timers.enable({ apis: ["setTimeout", "Date"] });
    await store.save(newTask("t1", "working"), callContext);

    for (let n = 1; n <= 5; n++) await store.save(newTask("t1", "working", { artifactText: `chunk ${n}` }), callContext);
    assert.equal(writesOf("t1").length, 1, "nothing is written inside the window");

    mock.timers.tick(500);
    await settle(() => writesOf("t1").length === 2);

    assert.equal(writesOf("t1").length, 2);
    assert.equal(server.tasks.get("t1").body.task.artifacts[0].parts[0].text, "chunk 5");
  });

  test("a status change flushes the pending update with it", async () => {
    mock.timers.enable({ apis: ["setTimeout", "Date"] });
    await store.save(newTask("t1", "working"), callContext);
    await store.save(newTask("t1", "working", { artifactText: "partial" }), callContext);

    await store.save(newTask("t1", "completed", { artifactText: "final" }), callContext);

    assert.equal(storedState("t1"), "completed");
    assert.equal(server.tasks.get("t1").body.task.artifacts[0].parts[0].text, "final");
    assert.equal(writesOf("t1").length, 2, "the pending update rode along, it was not written twice");
  });

  test("close writes what is still pending", async () => {
    mock.timers.enable({ apis: ["setTimeout", "Date"] });
    await store.save(newTask("t1", "working"), callContext);
    await store.save(newTask("t1", "working", { artifactText: "late" }), callContext);
    assert.equal(writesOf("t1").length, 1);

    await store.close();

    assert.equal(server.tasks.get("t1").body.task.artifacts[0].parts[0].text, "late");
  });

  test("the snapshot is taken at save: later in-place edits by the SDK are not stored", async () => {
    mock.timers.enable({ apis: ["setTimeout", "Date"] });
    const task = newTask("t1", "working");
    await store.save(task, callContext);
    task.history.push(userMessage("added after save"));
    await store.save(task, callContext);
    task.history.push(userMessage("added after the second save"));

    await store.close();

    assert.deepEqual(server.tasks.get("t1").body.task.history.map((m) => m.parts[0].text), ["added after save"]);
  });

  test("a failing state change rejects with the typed error", async () => {
    const tiny = await new FakeStateServer({ maxBodyBytes: 20 }).start();
    try {
      const small = new NasikoTaskStore(new NasikoStateClient(tiny.url, TOKEN, { retryBackoffMs: 0, fetch: tiny.fetch }), { logger });
      await assert.rejects(small.save(newTask("t1", "working"), callContext), NasikoStateTooLarge);
      await small.close();
    } finally {
      await tiny.stop();
    }
  });

  test("a transient failure rejects, and the owed write is retried in the background", async () => {
    mock.timers.enable({ apis: ["setTimeout", "Date"] });
    server.fail(500, { times: 3, when: (r) => r.method === "PUT" && r.path.startsWith("/tasks/") });

    await assert.rejects(store.save(newTask("t1", "working"), callContext), /HTTP 500/);
    assert.equal(server.tasks.has("t1"), false);

    mock.timers.tick(10_000);
    await settle(() => server.tasks.has("t1"));

    assert.equal(storedState("t1"), "working");
  });

  test("an update the platform could not take for over a minute is discarded", async () => {
    mock.timers.enable({ apis: ["setTimeout", "Date"] });
    await store.save(newTask("t1", "working"), callContext);
    await store.save(newTask("t1", "working", { artifactText: "stale" }), callContext);
    mock.timers.setTime(PENDING_MAX_AGE_MS + 1); // the clock moves without the flush timer firing

    await store.close();

    assert.equal((server.tasks.get("t1").body.task.artifacts ?? []).length, 0);
    assert.ok(logs.some(([level, m]) => level === "error" && /discarding/.test(m)));
  });
});

describe("replicas", () => {
  test("every write is conditional on the version this process last saw", async () => {
    await store.save(newTask("t1", "working"), callContext);
    await store.save(newTask("t1", "completed"), callContext);

    assert.equal(writesOf("t1")[0].headers["if-match"], undefined);
    assert.equal(writesOf("t1")[1].headers["if-match"], "1");
  });

  test("a state change that loses to another replica rejects, and load returns the other row", async () => {
    await store.save(newTask("t1", "working"), callContext);
    const theirs = encodeStoredTask(newTask("t1", "completed", { artifactText: "theirs" }));
    server.writeAsAnotherReplica("tasks", "t1", { task_id: "t1", context_id: "ctx-1", state: "completed", body: theirs });

    await assert.rejects(store.save(newTask("t1", "input-required"), callContext), NasikoStateConflict);
    const loaded = await store.load("t1", callContext);

    assert.equal(loaded.artifacts[0].parts[0].content.value, "theirs");
    assert.equal(loaded.status.state, TaskState.TASK_STATE_COMPLETED);
  });

  test("a coalesced update that loses is dropped and logged", async () => {
    mock.timers.enable({ apis: ["setTimeout", "Date"] });
    await store.save(newTask("t1", "working"), callContext);
    await store.save(newTask("t1", "working", { artifactText: "mine" }), callContext);
    const theirs = encodeStoredTask(newTask("t1", "completed"));
    server.writeAsAnotherReplica("tasks", "t1", { task_id: "t1", context_id: "ctx-1", state: "completed", body: theirs });

    mock.timers.tick(500);
    await settle(() => logs.length > 0);

    assert.equal(storedState("t1"), "completed");
    assert.ok(logs.some(([level, m]) => level === "warn" && /another writer/.test(m)));
  });

  test("load prefers this process's unwritten snapshot while it is based on the server row", async () => {
    mock.timers.enable({ apis: ["setTimeout", "Date"] });
    await store.save(newTask("t1", "working"), callContext);
    await store.save(newTask("t1", "working", { artifactText: "unwritten" }), callContext);

    const loaded = await store.load("t1", callContext);

    assert.equal(loaded.artifacts[0].parts[0].content.value, "unwritten");
  });

  test("a restarted replica reads what the first one wrote", async () => {
    await store.save(newTask("t1", "input-required", { artifactText: "waiting" }), callContext);

    const fresh = newStore();
    const loaded = await fresh.load("t1", callContext);
    await fresh.close();

    assert.equal(loaded.id, "t1");
    assert.equal(loaded.status.state, TaskState.TASK_STATE_INPUT_REQUIRED);
  });
});

describe("reading", () => {
  test("a missing task is undefined", async () => {
    assert.equal(await store.load("nope", callContext), undefined);
  });

  test("delete removes the row and an unwritten snapshot with it", async () => {
    mock.timers.enable({ apis: ["setTimeout", "Date"] });
    await store.save(newTask("t1", "working"), callContext);
    await store.save(newTask("t1", "working", { artifactText: "x" }), callContext);

    await store.delete("t1", callContext);
    await store.close();

    assert.equal(server.tasks.has("t1"), false);
  });

  test("a row in another format is refused, not half-decoded", async () => {
    server.writeAsAnotherReplica("tasks", "t1", {
      task_id: "t1",
      context_id: null,
      state: "working",
      body: { format: "a2a-0.3", task: { id: "t1" } },
    });

    await assert.rejects(store.load("t1", callContext), NasikoStateMalformedRow);
  });

  test("a body for a different task is refused", async () => {
    server.writeAsAnotherReplica("tasks", "t1", {
      task_id: "t1",
      context_id: null,
      state: "working",
      body: encodeStoredTask(newTask("other", "working")),
    });

    await assert.rejects(store.load("t1", callContext), /holds a body for task "other"/);
  });

  test("list filters by context and state, strips artifacts unless asked, and pages", async () => {
    await store.save(newTask("a", "working", { contextId: "c1", artifactText: "x" }), callContext);
    await store.save(newTask("b", "completed", { contextId: "c1" }), callContext);
    await store.save(newTask("c", "working", { contextId: "c2" }), callContext);

    const inContext = await store.list({ contextId: "c1" }, callContext);
    const working = await store.list({ contextId: "c1", status: TaskState.TASK_STATE_WORKING }, callContext);
    const withArtifacts = await store.list({ contextId: "c1", status: TaskState.TASK_STATE_WORKING, includeArtifacts: true }, callContext);
    const firstPage = await store.list({ pageSize: 2 }, callContext);
    const secondPage = await store.list({ pageSize: 2, pageToken: firstPage.nextPageToken }, callContext);

    assert.deepEqual(inContext.tasks.map((t) => t.id).sort(), ["a", "b"]);
    assert.deepEqual(working.tasks.map((t) => t.id), ["a"]);
    assert.equal(working.tasks[0].artifacts.length, 0);
    assert.equal(withArtifacts.tasks[0].artifacts.length, 1);
    assert.equal(firstPage.tasks.length, 2);
    assert.equal(secondPage.tasks.length, 1);
    assert.equal(secondPage.nextPageToken, "");
  });
});

describe("rows shared with the Python store", () => {
  test("a row written by the Python store decodes with every field intact", async () => {
    const row = fixture("python-task-a2a1.json");
    server.writeAsAnotherReplica("tasks", "task-1", { task_id: "task-1", context_id: "ctx-1", state: "input-required", body: row });

    const task = await store.load("task-1", callContext);

    assert.equal(task.contextId, "ctx-1");
    assert.equal(task.status.state, TaskState.TASK_STATE_INPUT_REQUIRED);
    assert.equal(task.status.timestamp, "2026-01-01T00:00:00.500Z");
    assert.equal(task.status.message.parts[0].content.value, "Which city?");
    assert.equal(task.history[0].parts[0].content.value, "weather");
    assert.equal(task.artifacts[0].name, "report");
    assert.deepEqual(task.metadata, { trace: "abc", n: 3 });
  });

  test("encoding that task again reproduces the Python row exactly", () => {
    const row = fixture("python-task-a2a1.json");

    assert.deepEqual(encodeStoredTask(Task.fromJSON(row.task)), row);
  });

  test("the row this store writes is the committed fixture the Python tests read", async () => {
    const task = Task.fromJSON(fixture("js-task-a2a1.json").task);
    await store.save(task, callContext);

    assert.deepEqual(server.tasks.get("task-js").body, fixture("js-task-a2a1.json"));
  });
});

describe("factory", () => {
  test("without NASIKO_STATE_URL it is the SDK in-memory store, and nothing is contacted", async () => {
    const memory = taskStoreFromEnv({ NASIKO_AGENT_TOKEN: TOKEN });

    assert.ok(memory instanceof InMemoryTaskStore);
    assert.equal(server.requests.length, 0);
  });

  test("with the url and token it is the durable store, and construction sends nothing", () => {
    const durable = taskStoreFromEnv({ NASIKO_STATE_URL: server.url, NASIKO_AGENT_TOKEN: TOKEN });

    assert.ok(durable instanceof NasikoTaskStore);
    assert.equal(server.requests.length, 0);
  });

  test("a url without a credential fails at startup", () => {
    assert.throws(() => taskStoreFromEnv({ NASIKO_STATE_URL: server.url }), /NASIKO_AGENT_TOKEN/);
  });
});

describe("shutdown", () => {
  test("closeOpenTaskStores flushes every open durable store", async () => {
    mock.timers.enable({ apis: ["setTimeout", "Date"] });
    await store.save(newTask("t1", "working"), callContext);
    await store.save(newTask("t1", "working", { artifactText: "pending" }), callContext);

    await closeOpenTaskStores(logger);

    assert.equal(server.tasks.get("t1").body.task.artifacts[0].parts[0].text, "pending");
  });

  test("the hook flushes on SIGTERM and exits with 128+15, and leaves a signal that has a listener alone", async () => {
    const before = process.listeners("SIGTERM");
    before.forEach((l) => process.removeListener("SIGTERM", l));
    const exited = new Promise((resolve) => installShutdownHooks({ exit: resolve }));
    try {
      process.emit("SIGTERM");
      assert.equal(await exited, 143);
      const listeners = process.listenerCount("SIGINT");
      installShutdownHooks({ exit: () => {} });
      assert.equal(process.listenerCount("SIGINT"), listeners, "a second install adds nothing for a handled signal");
    } finally {
      process.removeAllListeners("SIGTERM");
      process.removeAllListeners("SIGINT");
      before.forEach((l) => process.on("SIGTERM", l));
    }
  });
});
