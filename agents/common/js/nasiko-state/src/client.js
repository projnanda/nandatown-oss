/**
 * Typed client for the platform's state API (see oss/agents/common/nasiko_state.py for the
 * Python twin). One method per endpoint; failures raise the typed errors from errors.js.
 *
 * Connection errors and 5xx answers are retried (RETRY_ATTEMPTS attempts in total, exponential
 * backoff) because every call is idempotent; so is a 429, after the Retry-After it names when
 * that is at most MAX_RETRY_AFTER_SECONDS. Other 4xx answers never are. A conditional write whose
 * outcome is unknown (the request went out and no answer came back) is not repeated: it would
 * conflict with its own first attempt.
 *
 * The first write of each kind (tasks, kv) registers the agent with the platform, once.
 */

import {
  NasikoStateConflict,
  NasikoStateError,
  NasikoStateNotFound,
  NasikoStateRateLimited,
  NasikoStateTooLarge,
  NasikoStateUnauthorized,
} from "./errors.js";

export const STATE_URL_ENV = "NASIKO_STATE_URL";
export const AGENT_TOKEN_ENV = "NASIKO_AGENT_TOKEN";
export const CLIENT_NAME = "nasiko-state.js/1";

export const RETRY_ATTEMPTS = 3;
export const RETRY_BACKOFF_MS = 200;
export const REQUEST_TIMEOUT_MS = 10_000;
// The longest Retry-After a rate-limited request waits out; a longer one surfaces the 429.
export const MAX_RETRY_AFTER_SECONDS = 5;
export const MAX_PAGE_SIZE = 100;

// The order the registration contract spells its capability lists in.
const CAPABILITY_ORDER = ["tasks", "kv"];

// Failures that happen before the request is sent, so repeating a conditional write is safe.
const BEFORE_SEND_CODES = new Set(["ECONNREFUSED", "ENOTFOUND", "EAI_AGAIN", "UND_ERR_CONNECT_TIMEOUT"]);

/**
 * Escape everything but unreserved characters, so '/', ':' and '%' in an id stay inert. Matches
 * the Python store's encoding, which both use for URL segments and composite keys.
 * @param {string} value
 */
export function percentEncode(value) {
  return encodeURIComponent(value).replace(/[!'()*]/g, (c) => "%" + c.charCodeAt(0).toString(16).toUpperCase());
}

/**
 * @typedef {{version: number, updatedAt: string, expiresAt: string}} WriteReceipt
 * @typedef {{taskId: string, contextId: string | null, state: string, body: any, version: number,
 *   updatedAt: string, expiresAt: string}} TaskRecord
 * @typedef {{key: string, value: any, version: number, updatedAt: string, expiresAt: string}} KVRecord
 * @template T
 * @typedef {{items: T[], nextCursor: string | null}} Page
 */

export class NasikoStateClient {
  #baseUrl;
  #headers;
  #clientName;
  #fetch;
  #retryBackoffMs;
  #requestTimeoutMs;
  #registered = new Set();
  #registering = Promise.resolve();

  /**
   * @param {string} baseUrl base URL of the state API (ends in /api/agent-state)
   * @param {string} token this agent's state credential
   * @param {{clientName?: string, fetch?: typeof fetch, retryBackoffMs?: number,
   *   requestTimeoutMs?: number}} [options]
   */
  constructor(baseUrl, token, options = {}) {
    if (!/^https?:\/\/[^/]/.test(baseUrl)) {
      throw new NasikoStateError(`${STATE_URL_ENV} must be an http(s) URL, got ${JSON.stringify(baseUrl)}`);
    }
    if (!token) throw new NasikoStateError("the state API needs an agent token and none was given");
    this.#baseUrl = baseUrl.replace(/\/+$/, "");
    this.#clientName = options.clientName ?? CLIENT_NAME;
    this.#headers = { Authorization: `Bearer ${token}`, "User-Agent": this.#clientName };
    this.#fetch = options.fetch ?? globalThis.fetch;
    this.#retryBackoffMs = options.retryBackoffMs ?? RETRY_BACKOFF_MS;
    this.#requestTimeoutMs = options.requestTimeoutMs ?? REQUEST_TIMEOUT_MS;
  }

  /**
   * Build a client from the platform's environment, or null when it provides no state API.
   * @param {Record<string, string | undefined>} [env]
   * @returns {NasikoStateClient | null}
   */
  static fromEnv(env = process.env) {
    const url = (env[STATE_URL_ENV] ?? "").trim();
    if (!url) return null;
    const token = (env[AGENT_TOKEN_ENV] ?? "").trim();
    if (!token) {
      throw new NasikoStateError(
        `${STATE_URL_ENV} is set but ${AGENT_TOKEN_ENV} is empty or missing: the state API needs ` +
          `this agent's state credential, which the platform injects as ${AGENT_TOKEN_ENV}`,
      );
    }
    return new NasikoStateClient(url, token);
  }

  toString() {
    return `NasikoStateClient(${this.#baseUrl})`;
  }

  // --- registration -------------------------------------------------------------------------

  /**
   * Tell the platform which kinds of state this agent keeps. Writes do this on their own.
   * @param {string[]} capabilities
   * @param {string} [clientName]
   */
  async register(capabilities, clientName) {
    await this.#request("POST", "/register", {
      json: { capabilities: [...capabilities], client: clientName ?? this.#clientName },
      okStatus: 204,
    });
    for (const capability of capabilities) this.#registered.add(capability);
  }

  // --- tasks --------------------------------------------------------------------------------

  /**
   * Create or replace a task.
   * @param {string} taskId
   * @param {{contextId: string | null, state: string, body: any, ifMatch?: number | null}} fields
   * @returns {Promise<WriteReceipt>}
   */
  async putTask(taskId, { contextId, state, body, ifMatch = null }) {
    await this.#ensureRegistered("tasks");
    const data = await this.#request("PUT", `/tasks/${percentEncode(taskId)}`, {
      json: { context_id: contextId, state, body },
      ifMatch,
    });
    return receiptFrom(data);
  }

  /** @returns {Promise<TaskRecord | null>} */
  async getTask(taskId) {
    const data = await this.#request("GET", `/tasks/${percentEncode(taskId)}`, { whenMissing: null });
    return data === null ? null : taskRecordFrom(data);
  }

  /** Delete a task. False means there was no such task. */
  async deleteTask(taskId, { ifMatch = null } = {}) {
    await this.#ensureRegistered("tasks");
    return this.#delete(`/tasks/${percentEncode(taskId)}`, ifMatch);
  }

  /**
   * @param {{contextId?: string | null, state?: string | null, limit?: number, cursor?: string | null}} [filters]
   * @returns {Promise<Page<TaskRecord>>}
   */
  async listTasks({ contextId, state, limit, cursor } = {}) {
    const data = await this.#request("GET", "/tasks", {
      query: { context_id: contextId, state, limit, cursor },
    });
    return pageFrom(data, taskRecordFrom);
  }

  // --- key-value ----------------------------------------------------------------------------

  /**
   * @param {string} key
   * @param {any} value opaque JSON
   * @param {{ifMatch?: number | null}} [options]
   * @returns {Promise<WriteReceipt>}
   */
  async putKv(key, value, { ifMatch = null } = {}) {
    await this.#ensureRegistered("kv");
    const data = await this.#request("PUT", `/kv/${percentEncode(key)}`, { json: { value }, ifMatch });
    return receiptFrom(data);
  }

  /** @returns {Promise<KVRecord | null>} */
  async getKv(key) {
    const data = await this.#request("GET", `/kv/${percentEncode(key)}`, { whenMissing: null });
    return data === null ? null : kvRecordFrom(data);
  }

  /** Delete a key. False means there was no such key. */
  async deleteKv(key, { ifMatch = null } = {}) {
    await this.#ensureRegistered("kv");
    return this.#delete(`/kv/${percentEncode(key)}`, ifMatch);
  }

  /**
   * @param {{prefix?: string | null, limit?: number, cursor?: string | null}} [filters]
   * @returns {Promise<Page<KVRecord>>}
   */
  async listKv({ prefix, limit, cursor } = {}) {
    const data = await this.#request("GET", "/kv", { query: { prefix, limit, cursor } });
    return pageFrom(data, kvRecordFrom);
  }

  // --- execution ----------------------------------------------------------------------------

  async #delete(path, ifMatch) {
    const answer = await this.#request("DELETE", path, { ifMatch, okStatus: 204, whenMissing: false });
    return answer !== false;
  }

  /** Registers on the first write of a kind; concurrent first writes share one registration. */
  async #ensureRegistered(capability) {
    if (this.#registered.has(capability)) return;
    const registration = this.#registering.then(async () => {
      if (this.#registered.has(capability)) return;
      await this.register(this.#capabilitiesWith(capability));
    });
    // The chain must survive a failed registration, or every later write would reject with it.
    this.#registering = registration.catch(() => {});
    await registration;
  }

  #capabilitiesWith(capability) {
    const wanted = new Set([...this.#registered, capability]);
    const known = CAPABILITY_ORDER.filter((name) => wanted.has(name));
    return [...known, ...[...wanted].filter((name) => !CAPABILITY_ORDER.includes(name)).sort()];
  }

  /**
   * Send one request with retries and interpret the answer.
   * @returns {Promise<any>} the parsed JSON body, null for a 204, or `whenMissing` on a 404
   */
  async #request(method, path, { json, query, ifMatch = null, okStatus = 200, whenMissing } = {}) {
    const url = this.#baseUrl + path + queryString(query);
    const init = { method, headers: this.#requestHeaders(json !== undefined, ifMatch) };
    if (json !== undefined) init.body = JSON.stringify(json);
    const description = `state API ${method} ${path}`;

    for (let attempt = 1; ; attempt++) {
      const outcome = await this.#attempt(url, init);
      const delayMs = this.#retryDelayMs(attempt, outcome, ifMatch !== null);
      if (delayMs !== null) {
        if (delayMs > 0) await sleep(delayMs);
        continue;
      }
      if (outcome.error) {
        throw new NasikoStateError(`${description} request failed: ${outcome.error.message}`, {
          cause: outcome.error,
        });
      }
      return await interpret(outcome.response, description, okStatus, whenMissing);
    }
  }

  #requestHeaders(hasBody, ifMatch) {
    const headers = { ...this.#headers };
    if (hasBody) headers["Content-Type"] = "application/json";
    if (ifMatch !== null) headers["If-Match"] = String(ifMatch);
    return headers;
  }

  async #attempt(url, init) {
    try {
      const response = await this.#fetch(url, { ...init, signal: AbortSignal.timeout(this.#requestTimeoutMs) });
      // Read the body now, so a retry decision never races an unread stream.
      return { response: { status: response.status, headers: response.headers, text: await response.text() } };
    } catch (error) {
      return { error };
    }
  }

  /** Milliseconds to wait before trying again, or null when this outcome is the answer. */
  #retryDelayMs(attempt, outcome, conditional) {
    if (attempt >= RETRY_ATTEMPTS) return null;
    const backoff = this.#retryBackoffMs * 2 ** (attempt - 1);
    if (outcome.error) {
      return conditional && !failedBeforeSend(outcome.error) ? null : backoff;
    }
    const { status, headers } = outcome.response;
    if (status === 429) return retryAfterMs(headers, backoff);
    return status >= 500 ? backoff : null;
  }
}

function failedBeforeSend(error) {
  return BEFORE_SEND_CODES.has(error?.cause?.code);
}

function retryAfterMs(headers, defaultMs) {
  const header = headers.get("retry-after");
  const seconds = header === null ? defaultMs / 1000 : Number(header);
  // An HTTP-date is not worth parsing for a per-minute limit: treat it like a missing header.
  const usable = Number.isFinite(seconds) ? seconds : defaultMs / 1000;
  return usable >= 0 && usable <= MAX_RETRY_AFTER_SECONDS ? usable * 1000 : null;
}

async function interpret(response, description, okStatus, whenMissing) {
  if (response.status === 404 && whenMissing !== undefined) return whenMissing;
  if (response.status !== okStatus) throw errorFrom(response, description);
  if (okStatus === 204) return null;
  try {
    return JSON.parse(response.text);
  } catch (cause) {
    throw new NasikoStateError(`${description} returned invalid JSON (HTTP ${response.status})`, {
      status: response.status,
      body: response.text,
      cause,
    });
  }
}

function errorFrom(response, description) {
  const { status, text: body } = response;
  const message = `${description} failed with HTTP ${status}: ${problemText(body)}`;
  switch (status) {
    case 401:
      return new NasikoStateUnauthorized(message, { status, body });
    case 404:
      return new NasikoStateNotFound(message, { status, body });
    case 412:
      return new NasikoStateConflict(message, { currentVersion: currentVersionOf(body), body });
    case 413:
      return new NasikoStateTooLarge(message, { status, body });
    case 429: {
      const retryAfter = Number(response.headers.get("retry-after"));
      return new NasikoStateRateLimited(message, {
        retryAfterSeconds: Number.isFinite(retryAfter) ? retryAfter : null,
        body,
      });
    }
    default:
      return new NasikoStateError(message, { status, body });
  }
}

/** The server's own error text when the body is its JSON envelope, else the raw body. */
function problemText(body) {
  try {
    const data = JSON.parse(body);
    if (data && typeof data === "object") return String(data.error ?? data.message ?? body).slice(0, 200);
  } catch {
    // Not JSON: the raw body is the best description there is.
  }
  return body.slice(0, 200);
}

function currentVersionOf(body) {
  try {
    const version = JSON.parse(body)?.current_version;
    return Number.isInteger(version) ? version : null;
  } catch {
    return null;
  }
}

function queryString(query) {
  const entries = Object.entries(query ?? {}).filter(([, value]) => value !== undefined && value !== null);
  if (entries.length === 0) return "";
  return "?" + entries.map(([k, v]) => `${percentEncode(k)}=${percentEncode(String(v))}`).join("&");
}

function receiptFrom(data) {
  return field(data, "write receipt", () => ({
    version: data.version,
    updatedAt: data.updated_at,
    expiresAt: data.expires_at,
  }));
}

function taskRecordFrom(data) {
  return field(data, "task", () => ({
    taskId: data.task_id,
    contextId: data.context_id ?? null,
    state: data.state,
    body: data.body,
    version: data.version,
    updatedAt: data.updated_at,
    expiresAt: data.expires_at,
  }));
}

function kvRecordFrom(data) {
  return field(data, "key-value", () => ({
    key: data.key,
    value: data.value,
    version: data.version,
    updatedAt: data.updated_at,
    expiresAt: data.expires_at,
  }));
}

function pageFrom(data, parse) {
  if (!Array.isArray(data?.items)) {
    throw new NasikoStateError(`unexpected listing payload from the state API: ${JSON.stringify(data)?.slice(0, 200)}`);
  }
  return { items: data.items.map(parse), nextCursor: data.next_cursor ?? null };
}

function field(data, what, build) {
  if (data === null || typeof data !== "object" || !Number.isInteger(data.version)) {
    throw new NasikoStateError(`unexpected ${what} payload from the state API: ${JSON.stringify(data)?.slice(0, 200)}`);
  }
  return build();
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
