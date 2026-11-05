/**
 * An in-process HTTP implementation of the agent state API contract. It enforces what the
 * contract promises (bearer auth, strict bodies, If-Match, size limit, versions, cursor paging)
 * so the tests exercise the real wire behaviour over a real socket.
 */

import http from "node:http";

export const TOKEN = "test-agent-token";
const BASE_PATH = "/api/agent-state";

export class FakeStateServer {
  constructor({ maxBodyBytes = 1_048_576 } = {}) {
    this.maxBodyBytes = maxBodyBytes;
    this.requests = [];
    this.registrations = [];
    this.tasks = new Map();
    this.kv = new Map();
    this.injections = [];
    /** Awaited before a request is served; lets a test change state "between" two requests. */
    this.beforeServe = null;
    this.clock = 0;
    this.server = http.createServer((req, res) => void this.#handle(req, res));
  }

  async start() {
    await new Promise((resolve) => this.server.listen(0, "127.0.0.1", resolve));
    this.url = `http://127.0.0.1:${this.server.address().port}${BASE_PATH}`;
    return this;
  }

  async stop() {
    this.server.closeAllConnections();
    await new Promise((resolve) => this.server.close(resolve));
  }

  /** Answer the next `times` matching requests with `status` before touching any state. */
  fail(status, { times = 1, when = () => true, headers = {} } = {}) {
    this.injections.push({ status, times, when, headers });
  }

  writes(kind) {
    return this.requests.filter((r) => r.method === "PUT" && r.path.startsWith(`/${kind}/`));
  }

  /** Bump a stored row as if another replica wrote it. */
  writeAsAnotherReplica(table, id, value) {
    const rows = table === "tasks" ? this.tasks : this.kv;
    const existing = rows.get(id);
    rows.set(id, { ...value, version: (existing?.version ?? 0) + 1, seq: ++this.clock });
  }

  async #handle(req, res) {
    const chunks = [];
    for await (const chunk of req) chunks.push(chunk);
    const text = chunks.length ? Buffer.concat(chunks).toString() : null;
    const [status, body, headers] = await this.serve(req.method, req.url, req.headers, text);
    res.writeHead(status, { "Content-Type": "application/json", ...headers });
    res.end(body === null ? undefined : JSON.stringify(body));
  }

  /**
   * A fetch that reaches this server without a socket. Tests that fake the clock use it: mocked
   * timers also fire the HTTP stack's own timers, which wedges real connections.
   */
  fetch = async (url, init = {}) => {
    const headers = Object.fromEntries(Object.entries(init.headers ?? {}).map(([k, v]) => [k.toLowerCase(), v]));
    const [status, body, answerHeaders] = await this.serve(init.method ?? "GET", new URL(url).pathname + new URL(url).search, headers, init.body ?? null);
    return new Response(body === null ? null : JSON.stringify(body), { status, headers: answerHeaders });
  };

  async serve(method, target, headers, text) {
    const url = new URL(target, "http://fake");
    const recorded = {
      method,
      path: url.pathname.slice(BASE_PATH.length),
      query: Object.fromEntries(url.searchParams),
      headers,
      json: text ? JSON.parse(text) : null,
    };
    this.requests.push(recorded);
    if (this.beforeServe) await this.beforeServe(recorded);
    return this.#answer(recorded);
  }

  #answer(recorded) {
    const injection = this.injections.find((i) => i.times > 0 && i.when(recorded));
    if (injection) {
      injection.times -= 1;
      return [injection.status, { error: `injected ${injection.status}` }, injection.headers];
    }
    if (recorded.headers.authorization !== `Bearer ${TOKEN}`) return error(401, "missing or unknown agent token");
    const segments = recorded.path.split("/").slice(1).map(decodeURIComponent);
    const [kind, id] = segments;
    switch (`${recorded.method} ${segments.length === 1 ? kind : kind + "/:id"}`) {
      case "POST register":
        return this.#register(recorded);
      case "PUT tasks/:id":
        return this.#put(this.tasks, id, "task_id", recorded, ["context_id", "state", "body"], "body");
      case "PUT kv/:id":
        return this.#put(this.kv, id, "key", recorded, ["value"], "value");
      case "GET tasks/:id":
        return this.#get(this.tasks, id, "task_id");
      case "GET kv/:id":
        return this.#get(this.kv, id, "key");
      case "DELETE tasks/:id":
        return this.#delete(this.tasks, id, recorded);
      case "DELETE kv/:id":
        return this.#delete(this.kv, id, recorded);
      case "GET tasks":
        return this.#list(this.tasks, "task_id", recorded);
      case "GET kv":
        return this.#list(this.kv, "key", recorded);
      default:
        return error(404, `no route for ${recorded.method} ${recorded.path}`);
    }
  }

  #register(recorded) {
    const { capabilities, client } = recorded.json ?? {};
    if (!Array.isArray(capabilities) || capabilities.length === 0 || typeof client !== "string") {
      return error(400, "unknown or missing fields");
    }
    this.registrations.push(recorded.json);
    return [204, null, {}];
  }

  #put(rows, id, idName, recorded, fields, payloadField) {
    const body = recorded.json;
    if (!body || Object.keys(body).some((k) => !fields.includes(k))) return error(400, "unknown fields");
    if (Buffer.byteLength(JSON.stringify(body[payloadField])) > this.maxBodyBytes) return error(413, "body too large");
    const existing = rows.get(id);
    const conflict = versionConflict(existing, recorded);
    if (conflict) return conflict;
    const row = { ...body, [idName]: id, version: (existing?.version ?? 0) + 1, seq: ++this.clock };
    rows.set(id, row);
    return [200, { [idName]: id, ...stamps(row) }, {}];
  }

  #get(rows, id, idName) {
    const row = rows.get(id);
    return row ? [200, item(row, idName), {}] : error(404, "not found");
  }

  #delete(rows, id, recorded) {
    const existing = rows.get(id);
    if (!existing) return error(404, "not found");
    const conflict = versionConflict(existing, recorded);
    if (conflict) return conflict;
    rows.delete(id);
    return [204, null, {}];
  }

  #list(rows, idName, recorded) {
    const { prefix = "", context_id: contextId, state, cursor } = recorded.query;
    const limit = Number(recorded.query.limit ?? 50);
    if (!(limit >= 1 && limit <= 100)) return error(400, "limit must be 1..=100");
    const matching = [...rows.entries()].filter(
      ([id, row]) =>
        id.startsWith(prefix) &&
        (contextId === undefined || row.context_id === contextId) &&
        (state === undefined || row.state === state),
    );
    matching.sort(([a, x], [b, y]) => (idName === "key" ? a.localeCompare(b) : y.seq - x.seq || a.localeCompare(b)));
    const offset = cursor ? Number(Buffer.from(cursor, "base64url").toString()) : 0;
    const page = matching.slice(offset, offset + limit);
    const more = offset + limit < matching.length;
    return [
      200,
      {
        items: page.map(([, row]) => item(row, idName)),
        next_cursor: more ? Buffer.from(String(offset + limit)).toString("base64url") : null,
      },
      {},
    ];
  }
}

const error = (status, message) => [status, { error: message }, {}];

function versionConflict(existing, recorded) {
  const expected = recorded.headers["if-match"];
  if (expected === undefined || !existing || Number(expected) === existing.version) return null;
  return [412, { error: "version_conflict", current_version: existing.version }, {}];
}

function stamps(row) {
  return { version: row.version, updated_at: `2026-01-01T00:00:${String(row.seq % 60).padStart(2, "0")}Z`, expires_at: "2026-01-08T00:00:00Z" };
}

function item(row, idName) {
  const { seq: _seq, version: _version, ...fields } = row;
  return { ...(idName === "task_id" ? { context_id: null } : {}), ...fields, ...stamps(row) };
}
