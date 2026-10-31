"""An in-memory implementation of the agent state API contract, served through httpx.MockTransport.

It enforces what the contract promises (bearer auth, strict bodies, optimistic concurrency, size
and rate limits, versions, TTL fields, cursor paging) so the store tests exercise the same wire
behaviour the real server has, without a socket. The same transport serves sync and async clients.
"""

from __future__ import annotations

import base64
import json
from collections import deque
from collections.abc import Awaitable, Callable
from dataclasses import dataclass, field
from typing import Any
from urllib.parse import parse_qs, unquote

import httpx

BASE_PATH = "/api/agent-state"
BASE_URL = f"http://state.test{BASE_PATH}"
TOKEN = "test-agent-token"

DEFAULT_PAGE_SIZE = 50
MAX_PAGE_SIZE = 100
MAX_KEY_BYTES = 512
MAX_TASK_ID_BYTES = 256


@dataclass
class Row:
    value: dict[str, Any]
    version: int
    updated_seq: int


@dataclass(frozen=True)
class Recorded:
    method: str
    path: str
    query: dict[str, str]
    headers: httpx.Headers
    json: Any


@dataclass
class _Injection:
    times: int
    response: tuple[int, dict[str, Any], dict[str, str]] | None
    error: Exception | None
    when: Callable[[Recorded], bool]


@dataclass
class FakeStateServer:
    token: str = TOKEN
    max_body_bytes: int = 1_048_576
    rate_limit: int | None = None
    requests: list[Recorded] = field(default_factory=list)
    registrations: list[dict[str, Any]] = field(default_factory=list)
    tasks: dict[str, Row] = field(default_factory=dict)
    kv: dict[str, Row] = field(default_factory=dict)
    # Awaited before an async request is served; lets a test hold a request in flight.
    gate: Callable[[httpx.Request], Awaitable[None]] | None = None
    _injections: deque[_Injection] = field(default_factory=deque)
    _clock: int = 0

    def __post_init__(self) -> None:
        self.transport = _Transport(self)

    # --- test controls ---------------------------------------------------------------------

    def fail(
        self,
        status: int,
        *,
        times: int = 1,
        when: Callable[[Recorded], bool] | None = None,
        headers: dict[str, str] | None = None,
    ) -> None:
        """Answer the next `times` matching requests with `status` before touching any state."""
        response = (status, {"error": f"injected {status}"}, headers or {})
        self._injections.append(_Injection(times, response, None, when or (lambda _: True)))

    def disconnect(
        self,
        *,
        times: int = 1,
        when: Callable[[Recorded], bool] | None = None,
        error: Exception | None = None,
    ) -> None:
        """Raise a transport error (a refused connection by default) for the next `times` matching requests."""
        error = error or httpx.ConnectError("connection refused")
        self._injections.append(_Injection(times, None, error, when or (lambda _: True)))

    def writes(self, kind: str) -> list[Recorded]:
        """PUT requests to /tasks or /kv, in arrival order."""
        return [r for r in self.requests if r.method == "PUT" and r.path.split("/")[1] == kind]

    def calls(self, method: str, path_prefix: str) -> list[Recorded]:
        return [r for r in self.requests if r.method == method and r.path.startswith(path_prefix)]

    # --- request handling ------------------------------------------------------------------

    def handle(self, request: httpx.Request) -> httpx.Response:
        recorded = self._record(request)
        injected = self._injected(recorded)
        if injected is not None:
            return injected
        if request.headers.get("authorization") != f"Bearer {self.token}":
            return _error(401, "missing or unknown agent token")
        if self.rate_limit is not None and len(self.requests) > self.rate_limit:
            return httpx.Response(429, json={"error": "rate limited"}, headers={"Retry-After": "7"})
        return self._route(recorded, request)

    def _record(self, request: httpx.Request) -> Recorded:
        raw_path, _, raw_query = request.url.raw_path.decode().partition("?")
        path = raw_path.removeprefix(BASE_PATH)
        query = {k: v[0] for k, v in parse_qs(raw_query, keep_blank_values=True).items()}
        body = json.loads(request.content) if request.content else None
        recorded = Recorded(request.method, path, query, request.headers, body)
        self.requests.append(recorded)
        return recorded

    def _injected(self, recorded: Recorded) -> httpx.Response | None:
        for injection in list(self._injections):
            if injection.times <= 0 or not injection.when(recorded):
                continue
            injection.times -= 1
            if injection.error is not None:
                raise injection.error
            status, body, headers = injection.response  # type: ignore[misc]
            return httpx.Response(status, json=body, headers=headers)
        return None

    def _route(self, recorded: Recorded, request: httpx.Request) -> httpx.Response:
        segments = [unquote(part) for part in recorded.path.split("/")[1:]]
        match (recorded.method, segments):
            case ("POST", ["register"]):
                return self._register(recorded)
            case ("PUT", ["tasks", task_id]):
                return self._put_task(task_id, recorded)
            case ("GET", ["tasks", task_id]):
                return self._get(self.tasks, task_id, "task_id")
            case ("DELETE", ["tasks", task_id]):
                return self._delete(self.tasks, task_id, recorded)
            case ("GET", ["tasks"]):
                return self._list(self.tasks, "task_id", recorded, order="updated")
            case ("PUT", ["kv", key]):
                return self._put_kv(key, recorded)
            case ("GET", ["kv", key]):
                return self._get(self.kv, key, "key")
            case ("DELETE", ["kv", key]):
                return self._delete(self.kv, key, recorded)
            case ("GET", ["kv"]):
                return self._list(self.kv, "key", recorded, order="key")
        return _error(404, f"no route for {recorded.method} {recorded.path}")

    # --- endpoints -------------------------------------------------------------------------

    def _register(self, recorded: Recorded) -> httpx.Response:
        body = recorded.json
        if not _only_keys(body, {"capabilities", "client"}, required={"capabilities", "client"}):
            return _error(400, "unknown or missing fields")
        if not body["capabilities"] or not set(body["capabilities"]) <= {"tasks", "kv"}:
            return _error(400, "capabilities must be a non-empty subset of tasks, kv")
        self.registrations.append(body)
        return httpx.Response(204)

    def _put_task(self, task_id: str, recorded: Recorded) -> httpx.Response:
        if not 1 <= len(task_id.encode()) <= MAX_TASK_ID_BYTES:
            return _error(400, "task_id must be 1..=256 bytes")
        body = recorded.json
        if not _only_keys(body, {"context_id", "state", "body"}, required={"state", "body"}):
            return _error(400, "unknown or missing fields")
        return self._put(self.tasks, task_id, "task_id", body, payload=body["body"], recorded=recorded)

    def _put_kv(self, key: str, recorded: Recorded) -> httpx.Response:
        if not 1 <= len(key.encode()) <= MAX_KEY_BYTES or any(ord(c) < 32 for c in key):
            return _error(400, "key must be 1..=512 bytes of printable UTF-8")
        body = recorded.json
        if not _only_keys(body, {"value"}, required={"value"}):
            return _error(400, "unknown or missing fields")
        return self._put(self.kv, key, "key", body, payload=body["value"], recorded=recorded)

    def _put(
        self,
        table: dict[str, Row],
        row_id: str,
        id_name: str,
        body: dict[str, Any],
        *,
        payload: Any,
        recorded: Recorded,
    ) -> httpx.Response:
        if len(json.dumps(payload).encode()) > self.max_body_bytes:
            return _error(413, "body too large")
        existing = table.get(row_id)
        conflict = _version_conflict(existing, recorded)
        if conflict is not None:
            return conflict
        self._clock += 1
        version = existing.version + 1 if existing else 1
        table[row_id] = Row({id_name: row_id, **body}, version, self._clock)
        return httpx.Response(200, json={id_name: row_id, **self._stamps(version, self._clock)})

    def _get(self, table: dict[str, Row], row_id: str, id_name: str) -> httpx.Response:
        row = table.get(row_id)
        if row is None:
            return _error(404, "not found")
        return httpx.Response(200, json=self._item(row, id_name))

    def _delete(self, table: dict[str, Row], row_id: str, recorded: Recorded) -> httpx.Response:
        existing = table.get(row_id)
        if existing is None:
            return _error(404, "not found")
        conflict = _version_conflict(existing, recorded)
        if conflict is not None:
            return conflict
        del table[row_id]
        return httpx.Response(204)

    def _list(self, table: dict[str, Row], id_name: str, recorded: Recorded, *, order: str) -> httpx.Response:
        query = recorded.query
        limit = int(query.get("limit", DEFAULT_PAGE_SIZE))
        if not 1 <= limit <= MAX_PAGE_SIZE:
            return _error(400, "limit must be 1..=100")
        rows = [(row_id, row) for row_id, row in table.items() if _matches(id_name, row_id, row, query)]
        if order == "updated":
            rows.sort(key=lambda pair: (-pair[1].updated_seq, pair[0]))
        else:
            rows.sort(key=lambda pair: pair[0])
        offset = _decode_cursor(query.get("cursor"))
        if offset is None:
            return _error(400, "invalid cursor")
        page = rows[offset : offset + limit]
        more = offset + limit < len(rows)
        return httpx.Response(
            200,
            json={
                "items": [self._item(row, id_name) for _, row in page],
                "next_cursor": _encode_cursor(offset + limit) if more else None,
            },
        )

    def _item(self, row: Row, id_name: str) -> dict[str, Any]:
        item = dict(row.value)
        if id_name == "task_id":
            item.setdefault("context_id", None)
        return {**item, **self._stamps(row.version, row.updated_seq)}

    @staticmethod
    def _stamps(version: int, seq: int) -> dict[str, Any]:
        return {
            "version": version,
            "updated_at": f"2026-01-01T00:{seq // 60:02d}:{seq % 60:02d}Z",
            "expires_at": "2026-01-08T00:00:00Z",
        }


class _Transport(httpx.MockTransport):
    def __init__(self, server: FakeStateServer) -> None:
        super().__init__(server.handle)
        self._server = server

    async def handle_async_request(self, request: httpx.Request) -> httpx.Response:
        if self._server.gate is not None:
            await request.aread()
            await self._server.gate(request)
        return await super().handle_async_request(request)


def _error(status: int, message: str) -> httpx.Response:
    return httpx.Response(status, json={"error": message, "code": f"http_{status}"})


def _only_keys(body: Any, allowed: set[str], *, required: set[str]) -> bool:
    return isinstance(body, dict) and set(body) <= allowed and required <= set(body)


def _version_conflict(existing: Row | None, recorded: Recorded) -> httpx.Response | None:
    expected = recorded.headers.get("if-match")
    # Like the server: an absent row is at version 0, so If-Match: 0 means "only if absent".
    current = 0 if existing is None else existing.version
    if expected is None or int(expected) == current:
        return None
    return httpx.Response(412, json={"error": "version_conflict", "current_version": current})


def _matches(id_name: str, row_id: str, row: Row, query: dict[str, str]) -> bool:
    if id_name == "key":
        return row_id.startswith(query.get("prefix", ""))
    if "context_id" in query and row.value.get("context_id") != query["context_id"]:
        return False
    return "state" not in query or row.value.get("state") == query["state"]


def _encode_cursor(offset: int) -> str:
    return base64.urlsafe_b64encode(json.dumps({"offset": offset}).encode()).decode()


def _decode_cursor(cursor: str | None) -> int | None:
    if cursor is None:
        return 0
    try:
        return int(json.loads(base64.urlsafe_b64decode(cursor.encode()))["offset"])
    except (ValueError, KeyError, TypeError):
        return None
