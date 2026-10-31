/**
 * An a2a-js TaskStore that keeps tasks in the platform's state API. Port of NasikoTaskStore in
 * oss/agents/common/nasiko_state.py; the two read each other's rows.
 *
 * Written against @a2a-js/sdk 1.3.0, whose TaskStore is `save(task, context)`,
 * `load(taskId, context)` and `list(params, context)`. Tasks are protobuf-shaped there, and are
 * stored as their proto3 JSON, tagged with the format that wrote them:
 * `{"format": "a2a-1", "task": ...}`. The 0.3.x SDK is not supported: a row of another format is
 * refused with NasikoStateMalformedRow rather than half-decoded.
 *
 * Write guarantee: a save that changes `task.status.state` is on the server before `save()`
 * resolves. Other saves are coalesced to at most one write per COALESCE_WINDOW_MS per task and
 * written on the next state change, by a timer, and by `close()`. A hard crash can lose that
 * window of non-status updates; an update the platform could not take for PENDING_MAX_AGE_MS is
 * discarded rather than written late. Errors reject; nothing is swallowed.
 *
 * Replicas: every write is conditional on the version this process last saw of the task
 * (If-Match). A state change that loses raises NasikoStateConflict; a coalesced update that loses
 * is dropped (logged). Either way the store re-reads, and `load()` returns the other writer's row.
 *
 * Tasks are scoped to the agent, not to an end user: the SDK's per-user owner resolution (the
 * `context` argument) does not apply.
 */

import { Task, TaskState, taskStateToJSON } from "@a2a-js/sdk";
import { InMemoryTaskStore } from "@a2a-js/sdk/server";

import { NasikoStateClient } from "./client.js";
import {
  NasikoStateConflict,
  NasikoStateError,
  NasikoStateMalformedRow,
  isTransient,
} from "./errors.js";

export const TASK_FORMAT = "a2a-1";
export const COALESCE_WINDOW_MS = 500;
// A deferred write that failed is retried after a delay that doubles per failure, up to this.
export const DEFERRED_RETRY_MAX_MS = 10_000;
// An unwritten update this old is presumed superseded (the task has most likely moved to another
// replica), so it is discarded instead of being written over whatever is there now.
export const PENDING_MAX_AGE_MS = 60_000;
// Bound on tasks whose write state is remembered. Forgetting an idle one costs its next write the
// version check: having no version to match, that write is unconditional.
export const MAX_TRACKED_TASKS = 4096;
export const DEFAULT_PAGE_SIZE = 50;

/** Durable stores this process opened and has not closed: what closeOpenTaskStores closes. */
const openStores = new Set();

// --- format ---------------------------------------------------------------------------------------

/** 'TASK_STATE_INPUT_REQUIRED' -> 'input-required', the vocabulary the state column uses. */
export function stateName(state) {
  const name = taskStateToJSON(state).replace(/^TASK_STATE_/, "").toLowerCase().replaceAll("_", "-");
  return name === "unspecified" || name === "unrecognized" ? "unknown" : name;
}

/** The body written to the state API: the task as proto3 JSON, tagged with its format. */
export function encodeStoredTask(task) {
  // Through JSON, so the snapshot is plain data that shares nothing with the SDK's live object.
  return JSON.parse(JSON.stringify({ format: TASK_FORMAT, task: Task.toJSON(task) }));
}

export function decodeStoredTask(body, taskId) {
  const storedFormat = body !== null && typeof body === "object" ? body.format : undefined;
  if (storedFormat !== TASK_FORMAT || !("task" in body)) {
    throw new NasikoStateMalformedRow(
      `stored task ${JSON.stringify(taskId)} is in format ${JSON.stringify(storedFormat)}, and this ` +
        `store reads ${JSON.stringify(TASK_FORMAT)} only`,
    );
  }
  let task;
  try {
    task = Task.fromJSON(body.task);
  } catch (cause) {
    throw new NasikoStateMalformedRow(`stored task ${JSON.stringify(taskId)} cannot be decoded: ${cause.message}`, {
      cause,
    });
  }
  // fromJSON turns {} into an empty task rather than failing, so check the identity too.
  if (task.id !== taskId) {
    throw new NasikoStateMalformedRow(
      `stored task ${JSON.stringify(taskId)} holds a body for task ${JSON.stringify(task.id)}`,
    );
  }
  return task;
}

function snapshotOf(task) {
  return {
    taskId: task.id,
    contextId: task.contextId || null,
    state: task.status ? stateName(task.status.state) : "unknown",
    body: encodeStoredTask(task),
    savedAt: Date.now(),
  };
}

const isStale = (snapshot) => Date.now() - snapshot.savedAt > PENDING_MAX_AGE_MS;

// --- per-task bookkeeping -------------------------------------------------------------------------

/** Runs one async function at a time, in call order. */
class Mutex {
  #tail = Promise.resolve();

  isLocked = false;

  runExclusive(fn) {
    const result = this.#tail.then(async () => {
      this.isLocked = true;
      try {
        return await fn();
      } finally {
        this.isLocked = false;
      }
    });
    this.#tail = result.catch(() => {});
    return result;
  }
}

/** Write-coalescing and concurrency bookkeeping for one task. */
class TaskSlot {
  constructor(taskId) {
    this.taskId = taskId;
    /** The newest snapshot the server has not confirmed yet. */
    this.pending = null;
    /** The row's version as this process last saw it. Every write is conditional on it. */
    this.version = null;
    /** State of the newest snapshot sent or read; a save in any other state is written through. */
    this.state = null;
    this.lastWriteStarted = Number.NEGATIVE_INFINITY;
    /** Consecutive failed writes; spaces out the background retries. */
    this.failedWrites = 0;
    this.timer = null;
    /** One write per task at a time, so an older snapshot cannot land after a newer one. */
    this.lock = new Mutex();
  }

  isIdle() {
    return this.pending === null && this.timer === null && !this.lock.isLocked;
  }
}

// --- the store ------------------------------------------------------------------------------------

export class NasikoTaskStore {
  #client;
  #windowMs;
  #logger;
  /** @type {Map<string, TaskSlot>} insertion order is recency order */
  #slots = new Map();

  /**
   * @param {NasikoStateClient} client
   * @param {{coalesceWindowMs?: number, logger?: Pick<Console, "warn" | "error">}} [options]
   */
  constructor(client, { coalesceWindowMs = COALESCE_WINDOW_MS, logger = console } = {}) {
    this.#client = client;
    this.#windowMs = coalesceWindowMs;
    this.#logger = logger;
    openStores.add(this);
  }

  /** @param {import("@a2a-js/sdk").Task} task */
  async save(task, _context) {
    const snapshot = snapshotOf(task);
    const slot = this.#slot(snapshot.taskId);
    slot.pending = snapshot;
    if (snapshot.state === slot.state) {
      this.#scheduleFlush(slot);
      return;
    }
    slot.state = snapshot.state;
    try {
      await this.#writePending(slot);
    } catch (error) {
      // The state change is still owed; retry it rather than wait for a save that may never come.
      if (isTransient(error)) this.#scheduleFlush(slot);
      throw error;
    }
  }

  async load(taskId, _context) {
    const slot = this.#slot(taskId);
    // Under the slot's lock, so a write in flight cannot be mistaken for another writer's.
    return slot.lock.runExclusive(async () => {
      const record = await this.#client.getTask(taskId);
      return this.#newestOf(slot, record);
    });
  }

  async delete(taskId, _context) {
    const slot = this.#slot(taskId);
    await slot.lock.runExclusive(async () => {
      slot.pending = null; // an unwritten snapshot must not bring the task back
      await this.#client.deleteTask(taskId);
    });
    if (this.#slots.get(taskId) === slot) this.#slots.delete(taskId);
  }

  /**
   * tasks/list, served by the server; it can lag by one coalescing window. totalSize is unknown
   * and reported as 0 (the state API gives no totals). statusTimestampAfter is inclusive (>=),
   * like the Python store and unlike the SDK's in-memory store.
   */
  async list(params, _context) {
    const limit = params.pageSize || DEFAULT_PAGE_SIZE;
    const page = await this.#client.listTasks({
      contextId: params.contextId || null,
      state: params.status ? stateName(params.status) : null,
      limit,
      cursor: params.pageToken || null,
    });
    const tasks = page.items
      .map((record) => decodeStoredTask(record.body, record.taskId))
      .filter((task) => statusChangedSince(task, params.statusTimestampAfter))
      .map((task) => shaped(task, params));
    // totalSize stays 0: the state API reports no totals, and this page's size would be a wrong one.
    return { tasks, nextPageToken: page.nextCursor ?? "", pageSize: tasks.length, totalSize: 0 };
  }

  /** Write everything pending, stop the timers. Rejects with the first failure, after trying all. */
  async close() {
    openStores.delete(this);
    for (const slot of this.#slots.values()) this.#clearTimer(slot);
    const owed = [...this.#slots.values()].filter((slot) => this.#owesAWrite(slot));
    const outcomes = await Promise.allSettled(owed.map((slot) => this.#writePending(slot)));
    const failures = [];
    outcomes.forEach((outcome, i) => {
      if (outcome.status === "fulfilled") return;
      if (outcome.reason instanceof NasikoStateConflict) this.#logSuperseded(owed[i], outcome.reason);
      else failures.push(outcome.reason);
    });
    for (const failure of failures.slice(1)) {
      this.#logger.error(`a pending task write could not be flushed on close: ${failure.message}`);
    }
    if (failures.length > 0) throw failures[0];
  }

  // --- writing ----------------------------------------------------------------------------------

  #writePending(slot) {
    return slot.lock.runExclusive(async () => {
      const snapshot = slot.pending;
      if (snapshot === null) return; // a newer write already carried it
      slot.lastWriteStarted = Date.now();
      let receipt;
      try {
        receipt = await this.#client.putTask(snapshot.taskId, {
          contextId: snapshot.contextId,
          state: snapshot.state,
          body: snapshot.body,
          ifMatch: slot.version,
        });
      } catch (error) {
        if (error instanceof NasikoStateConflict) await this.#rereadAfterConflict(slot);
        else if (error instanceof NasikoStateError) this.#recordFailure(slot, error);
        throw error;
      }
      this.#confirm(slot, snapshot, receipt.version);
    });
  }

  #confirm(slot, snapshot, version) {
    slot.version = version;
    slot.failedWrites = 0;
    if (slot.state === null) slot.state = snapshot.state;
    // Otherwise a newer snapshot arrived while this one was in flight; it is still owed.
    if (slot.pending === snapshot) slot.pending = null;
  }

  #recordFailure(slot, error) {
    slot.failedWrites += 1;
    slot.state = null; // unknown whether the server has it, so the next save writes through
    if (!isTransient(error)) slot.pending = null; // retrying cannot help; the caller has the error
  }

  /** Another writer changed the task first: drop what this process had not written, re-read. */
  async #rereadAfterConflict(slot) {
    slot.pending = null;
    slot.state = null;
    let record;
    try {
      record = await this.#client.getTask(slot.taskId);
    } catch (error) {
      // The stale version is kept, so the next write conflicts again and retries the read.
      this.#logger.error(`re-reading task ${slot.taskId} after a write conflict failed: ${error.message}`);
      return;
    }
    this.#adopt(slot, record);
  }

  // --- reading ----------------------------------------------------------------------------------

  /** This process's unwritten snapshot while it is based on the server's row, else the row. */
  #newestOf(slot, record) {
    if (slot.pending !== null && (record === null || record.version === slot.version)) {
      return decodeStoredTask(slot.pending.body, slot.taskId);
    }
    if (record === null) return undefined;
    const task = decodeStoredTask(record.body, slot.taskId);
    if (slot.pending !== null) {
      this.#logger.warn(
        `task ${slot.taskId} was changed by another writer (version ${record.version}); ` +
          "this replica's unwritten update was dropped",
      );
      slot.pending = null;
    }
    this.#adopt(slot, record);
    return task;
  }

  #adopt(slot, record) {
    slot.version = record === null ? null : record.version;
    slot.state = record === null ? null : record.state;
  }

  // --- coalescing -------------------------------------------------------------------------------

  #scheduleFlush(slot) {
    if (slot.timer !== null) return;
    slot.timer = setTimeout(() => void this.#flushWhenDue(slot), this.#msUntilDue(slot));
  }

  #clearTimer(slot) {
    if (slot.timer === null) return;
    clearTimeout(slot.timer);
    slot.timer = null;
  }

  async #flushWhenDue(slot) {
    slot.timer = null;
    if (!this.#owesAWrite(slot)) return;
    await this.#tryDeferredWrite(slot);
    if (this.#owesAWrite(slot)) this.#scheduleFlush(slot);
  }

  async #tryDeferredWrite(slot) {
    // No caller awaits a deferred write to hand its error to, so each outcome is logged here.
    try {
      await this.#writePending(slot);
    } catch (error) {
      if (error instanceof NasikoStateConflict) return this.#logSuperseded(slot, error);
      const retry = slot.pending !== null ? "will retry" : "dropped";
      this.#logger.error(`deferred write of task ${slot.taskId} failed (${retry}): ${error.message}`);
    }
  }

  #msUntilDue(slot) {
    const delay = Math.min(this.#windowMs * 2 ** slot.failedWrites, Math.max(this.#windowMs, DEFERRED_RETRY_MAX_MS));
    return Math.max(0, slot.lastWriteStarted + delay - Date.now());
  }

  /** Whether the slot has an update to write, discarding it instead when it is stale. */
  #owesAWrite(slot) {
    if (slot.pending === null) return false;
    if (!isStale(slot.pending)) return true;
    this.#logger.error(
      `task ${slot.taskId}: discarding an update that could not be written for over ` +
        `${PENDING_MAX_AGE_MS / 1000} s`,
    );
    slot.pending = null;
    return false;
  }

  #logSuperseded(slot, conflict) {
    this.#logger.warn(
      `task ${slot.taskId} was changed by another writer (now version ${conflict.currentVersion}); ` +
        "this replica's unwritten update was dropped",
    );
  }

  // --- slot bookkeeping -------------------------------------------------------------------------

  #slot(taskId) {
    const existing = this.#slots.get(taskId);
    if (existing !== undefined) {
      this.#slots.delete(taskId); // re-insert: most recently used goes last
      this.#slots.set(taskId, existing);
      return existing;
    }
    const slot = new TaskSlot(taskId);
    this.#slots.set(taskId, slot);
    this.#evictIdleSlots();
    return slot;
  }

  /** Keep memory bounded: drop the least recently used slots that owe the server nothing. */
  #evictIdleSlots() {
    let excess = this.#slots.size - MAX_TRACKED_TASKS;
    for (const [taskId, slot] of this.#slots) {
      if (excess <= 0) return;
      if (slot.isIdle()) {
        this.#slots.delete(taskId);
        excess -= 1;
      }
    }
  }
}

// --- list helpers ---------------------------------------------------------------------------------

/** tasks/list's statusTimestampAfter, applied here: the state API cannot filter on it. */
function statusChangedSince(task, after) {
  if (!after) return true;
  if (!task.status?.timestamp) return false;
  return Date.parse(task.status.timestamp) >= Date.parse(after);
}

/** Apply tasks/list's per-task shaping: artifacts only when asked for, history trimmed. */
function shaped(task, params) {
  const shapedTask = { ...task };
  if (!params.includeArtifacts) shapedTask.artifacts = [];
  if (params.historyLength !== undefined && params.historyLength >= 0) {
    shapedTask.history = params.historyLength === 0 ? [] : task.history.slice(-params.historyLength);
  }
  return shapedTask;
}

// --- factory and shutdown -------------------------------------------------------------------------

/**
 * The task store an agent hands to its request handler: the platform's durable store when
 * NASIKO_STATE_URL is set, otherwise the SDK's in-memory store, built exactly as an agent would
 * build it by hand. Nothing contacts the platform until the first task is saved.
 * @param {Record<string, string | undefined>} [env]
 */
export function taskStoreFromEnv(env = process.env) {
  const client = NasikoStateClient.fromEnv(env);
  if (client === null) return new InMemoryTaskStore();
  return new NasikoTaskStore(client);
}

/** Close (and so flush) every durable task store this process opened. */
export async function closeOpenTaskStores(logger = console) {
  for (const store of [...openStores]) {
    try {
      await store.close();
    } catch (error) {
      logger.error(`task state could not be flushed at shutdown: ${error.message}`);
    }
  }
}

/**
 * Flush durable task state on SIGTERM and SIGINT, which Node otherwise answers by dying with
 * writes pending. A signal that already has a listener is left to it. Call once at startup.
 * @param {{exit?: (code: number) => void}} [options]
 */
export function installShutdownHooks({ exit = (code) => process.exit(code) } = {}) {
  for (const [signal, number] of [["SIGTERM", 15], ["SIGINT", 2]]) {
    if (process.listenerCount(signal) > 0) continue;
    process.once(signal, () => void closeOpenTaskStores().finally(() => exit(128 + number)));
  }
}

export { TaskState };
