import assert from "node:assert/strict";
import { afterEach, beforeEach, describe, test } from "node:test";

import {
  NasikoStateClient,
  NasikoStateConflict,
  NasikoStateError,
  NasikoStateRateLimited,
  NasikoStateTooLarge,
  NasikoStateUnauthorized,
} from "../src/index.js";
import { FakeStateServer, TOKEN } from "./fake-state-server.js";

let server;
let client;

beforeEach(async () => {
  server = await new FakeStateServer().start();
  client = new NasikoStateClient(server.url, TOKEN, { retryBackoffMs: 0 });
});
afterEach(() => server.stop());

describe("tasks", () => {
  test("put, get, list and delete round trip", async () => {
    const receipt = await client.putTask("t1", { contextId: "c1", state: "working", body: { a: [1, null] } });
    const record = await client.getTask("t1");
    const page = await client.listTasks({ contextId: "c1" });

    assert.equal(receipt.version, 1);
    assert.deepEqual(
      { taskId: record.taskId, contextId: record.contextId, state: record.state, body: record.body },
      { taskId: "t1", contextId: "c1", state: "working", body: { a: [1, null] } },
    );
    assert.deepEqual(page.items.map((r) => r.taskId), ["t1"]);
    assert.equal(page.nextCursor, null);
    assert.equal(await client.deleteTask("t1"), true);
    assert.equal(await client.getTask("t1"), null);
    assert.equal(await client.deleteTask("t1"), false);
  });

  test("an id with reserved characters stays one path segment", async () => {
    await client.putTask("a/b:c%d", { contextId: null, state: "working", body: {} });

    assert.ok(server.tasks.has("a/b:c%d"));
    assert.equal(server.requests.at(-1).path, "/tasks/a%2Fb%3Ac%25d");
  });

  test("the state filter reaches the server", async () => {
    await client.putTask("t1", { contextId: "c", state: "working", body: {} });
    await client.putTask("t2", { contextId: "c", state: "completed", body: {} });

    const page = await client.listTasks({ state: "completed" });

    assert.deepEqual(page.items.map((r) => r.taskId), ["t2"]);
  });
});

describe("key-value", () => {
  test("put, get and delete round trip", async () => {
    await client.putKv("greeting", { hello: "world" });

    assert.deepEqual((await client.getKv("greeting")).value, { hello: "world" });
    assert.equal(await client.deleteKv("greeting"), true);
    assert.equal(await client.getKv("greeting"), null);
  });

  test("listing follows cursors and honours the prefix", async () => {
    for (let n = 0; n < 130; n++) await client.putKv(`item:${String(n).padStart(3, "0")}`, n);
    await client.putKv("other", 0);

    const keys = [];
    let cursor = null;
    do {
      const page = await client.listKv({ prefix: "item:", limit: 100, cursor });
      keys.push(...page.items.map((r) => r.key));
      cursor = page.nextCursor;
    } while (cursor !== null);

    assert.equal(keys.length, 130);
    assert.ok(keys.every((key) => key.startsWith("item:")));
  });
});

describe("error mapping", () => {
  test("412 is a conflict that names the current version", async () => {
    await client.putKv("k", 1);
    await client.putKv("k", 2);

    await assert.rejects(client.putKv("k", 3, { ifMatch: 1 }), (error) => {
      assert.ok(error instanceof NasikoStateConflict);
      assert.equal(error.currentVersion, 2);
      assert.equal(error.status, 412);
      return true;
    });
  });

  test("If-Match is sent as the integer version", async () => {
    await client.putKv("k", 1);
    await client.putKv("k", 2, { ifMatch: 1 });

    assert.equal(server.requests.at(-1).headers["if-match"], "1");
  });

  test("401 is unauthorized and is not retried", async () => {
    const stranger = new NasikoStateClient(server.url, "wrong", { retryBackoffMs: 0 });

    await assert.rejects(stranger.getKv("k"), NasikoStateUnauthorized);
    assert.equal(server.requests.filter((r) => r.method === "GET").length, 1);
  });

  test("413 is too large and is not retried", async () => {
    const small = await new FakeStateServer({ maxBodyBytes: 10 }).start();
    try {
      const tiny = new NasikoStateClient(small.url, TOKEN, { retryBackoffMs: 0 });
      await assert.rejects(tiny.putKv("k", "x".repeat(50)), NasikoStateTooLarge);
      assert.equal(small.writes("kv").length, 1);
    } finally {
      await small.stop();
    }
  });

  test("a 429 that asks for a long wait is surfaced", async () => {
    server.fail(429, { times: 5, headers: { "Retry-After": "60" } });

    await assert.rejects(client.getKv("k"), (error) => {
      assert.ok(error instanceof NasikoStateRateLimited);
      assert.equal(error.retryAfterSeconds, 60);
      return true;
    });
    assert.equal(server.requests.length, 1);
  });

  test("a 429 that asks for a short wait is retried", async () => {
    server.fail(429, { headers: { "Retry-After": "0" } });

    assert.equal(await client.getKv("k"), null);
    assert.equal(server.requests.length, 2);
  });

  test("5xx answers are retried, then surfaced", async () => {
    server.fail(503, { times: 1 });
    assert.equal(await client.getKv("k"), null);

    server.requests.length = 0;
    server.fail(500, { times: 10 });
    await assert.rejects(client.getKv("k"), (error) => error instanceof NasikoStateError && error.status === 500);
    assert.equal(server.requests.length, 3);
  });

  test("an unreachable platform is a NasikoStateError with no status", async () => {
    const dead = new NasikoStateClient("http://127.0.0.1:1/api/agent-state", TOKEN, { retryBackoffMs: 0 });

    await assert.rejects(dead.getKv("k"), (error) => error instanceof NasikoStateError && error.status === null);
  });

  test("a url that is not http(s) and a missing token fail at construction", () => {
    assert.throws(() => new NasikoStateClient("ftp://x", TOKEN), NasikoStateError);
    assert.throws(() => new NasikoStateClient(server.url, ""), NasikoStateError);
  });
});

describe("registration", () => {
  test("the first task write registers tasks only", async () => {
    await client.putTask("t", { contextId: null, state: "working", body: {} });

    assert.deepEqual(server.registrations.map((r) => r.capabilities), [["tasks"]]);
  });

  test("the first kv write registers kv only, and a later task write widens it", async () => {
    await client.putKv("k", 1);
    await client.putTask("t", { contextId: null, state: "working", body: {} });
    await client.putKv("k", 2);

    assert.deepEqual(server.registrations.map((r) => r.capabilities), [["kv"], ["tasks", "kv"]]);
  });

  test("reads never register", async () => {
    await client.getTask("t");
    await client.listKv();

    assert.equal(server.registrations.length, 0);
  });

  test("concurrent first writes register once", async () => {
    await Promise.all([1, 2, 3].map((n) => client.putKv(`k${n}`, n)));

    assert.equal(server.registrations.length, 1);
  });
});

describe("fromEnv", () => {
  test("is null without a url, even with a token", () => {
    assert.equal(NasikoStateClient.fromEnv({}), null);
    assert.equal(NasikoStateClient.fromEnv({ NASIKO_STATE_URL: "  ", NASIKO_AGENT_TOKEN: TOKEN }), null);
    assert.equal(NasikoStateClient.fromEnv({ NASIKO_AGENT_TOKEN: TOKEN }), null);
  });

  test("a url without NASIKO_AGENT_TOKEN fails, and the gateway token is no substitute", () => {
    assert.throws(
      () => NasikoStateClient.fromEnv({ NASIKO_STATE_URL: server.url, MCP_GATEWAY_TOKEN: TOKEN }),
      /NASIKO_AGENT_TOKEN/,
    );
  });

  test("builds a client that carries the token", async () => {
    const built = NasikoStateClient.fromEnv({ NASIKO_STATE_URL: server.url, NASIKO_AGENT_TOKEN: TOKEN });

    await built.putKv("k", 1);

    assert.equal(server.requests.at(-1).headers.authorization, `Bearer ${TOKEN}`);
  });
});
