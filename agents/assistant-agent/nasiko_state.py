"""Durable agent state over the platform's state API.

An A2A agent normally keeps its tasks in process memory, so a restart loses paused work
and the agent cannot run as more than one replica. This module is a drop-in replacement
that keeps that state in the platform instead.

    from nasiko_state import make_task_store

    handler = DefaultRequestHandler(
        agent_executor=MyExecutor(),
        task_store=make_task_store(),   # was: InMemoryTaskStore()
        agent_card=card,
    )
    app = Starlette(routes=...)
    install_shutdown_hooks(app)         # flush pending writes when the server stops

Configuration comes from the environment the platform injects into the container:

    NASIKO_STATE_URL     base URL of the state API (ends in /api/agent-state). Unset means the
                         platform provides no store, and every factory here returns the
                         in-memory implementation the SDK ships, so the same code runs locally.
    NASIKO_AGENT_TOKEN   this agent's state credential. It is the only one accepted: the MCP
                         gateway token is a different credential and is never used here.

Write guarantee (see NasikoTaskStore): a save that changes ``task.status.state`` is on the
server before ``save()`` returns. Other saves are coalesced to at most one write per
COALESCE_WINDOW_SECONDS per task and written on the next state change, by a background timer,
and by ``aclose()``. Nothing calls ``aclose()`` on its own: ``install_shutdown_hooks(app)`` runs
it when the server shuts down (uvicorn does that on SIGTERM and Ctrl-C), and an atexit hook
covers a normal interpreter exit. A hard crash (SIGKILL, power loss), or a SIGTERM in a process
without those hooks, can lose that window of non-status updates; so can an update the platform
could not take for PENDING_MAX_AGE_SECONDS, which is discarded rather than written late.

Both a2a-sdk majors are supported (0.3.x pydantic tasks and 1.x protobuf tasks). The langgraph
checkpointer needs the optional langgraph-checkpoint package; nothing else does.

This file is vendored: agent images are built from the agent's own directory only, so each
agent keeps a committed copy next to its main module. oss/agents/common is canonical and
oss/agents/common/check_vendored.py fails when a copy drifts.
"""

from __future__ import annotations

import asyncio
import atexit
import base64
import contextlib
import functools
import json
import logging
import os
import random
import signal
import threading
import time
from collections import OrderedDict
from collections.abc import AsyncIterator, Callable, Iterator, Mapping, Sequence
from dataclasses import dataclass, fields
from typing import Any, Generic, TypeVar
from urllib.parse import quote, urlsplit

import httpx
from a2a.server.tasks import InMemoryTaskStore, TaskStore
from a2a.types import Task

try:
    from a2a.types import a2a_pb2 as _a2a_pb2  # a2a-sdk 1.x: tasks are protobuf messages
except ImportError:
    _a2a_pb2 = None  # a2a-sdk 0.3.x: tasks are pydantic models

try:
    from langgraph.checkpoint.base import (
        WRITES_IDX_MAP,
        BaseCheckpointSaver,
        CheckpointTuple,
        get_checkpoint_metadata,
    )

    _LANGGRAPH_AVAILABLE = True
except ImportError:
    # langgraph is optional: only agents that checkpoint with it need the package.
    BaseCheckpointSaver = object  # type: ignore[assignment,misc]
    _LANGGRAPH_AVAILABLE = False

logger = logging.getLogger("nasiko_state")

STATE_URL_ENV = "NASIKO_STATE_URL"
AGENT_TOKEN_ENV = "NASIKO_AGENT_TOKEN"

CLIENT_NAME = "nasiko_state.py/1"

CONNECT_TIMEOUT_SECONDS = 2.0
READ_TIMEOUT_SECONDS = 10.0
RETRY_ATTEMPTS = 3
RETRY_BACKOFF_SECONDS = 0.2
# A rate-limited (429) request is retried after the Retry-After the server names, waiting at most
# MAX_RETRY_AFTER_SECONDS per attempt and RATE_LIMIT_BUDGET_SECONDS in all before the 429 surfaces.
# The platform's limit is a per-minute window shared by every replica of the agent, so a wait of a
# few seconds is usually enough, and a turn that keeps failing for half a minute should fail.
MAX_RETRY_AFTER_SECONDS = 5.0
RATE_LIMIT_BUDGET_SECONDS = 30.0
# The wait when a 429 names none (or names an HTTP date), and the shortest wait ever taken, so a
# server answering "Retry-After: 0" cannot turn the budget into a busy loop.
RATE_LIMIT_DEFAULT_WAIT_SECONDS = 1.0
RATE_LIMIT_MIN_WAIT_SECONDS = 0.5
# Relative spread of each 429 wait, so replicas limited at the same moment do not retry in step.
RATE_LIMIT_JITTER = 0.2
MAX_PAGE_SIZE = 100
DEFAULT_PAGE_SIZE = 50

# Saves that do not change a task's state are written at most once per window per task.
COALESCE_WINDOW_SECONDS = 0.5
# A deferred write that failed is retried after a delay that doubles per failure, up to this.
DEFERRED_RETRY_MAX_SECONDS = 10.0
# An unwritten update this old is presumed superseded (the task has most likely moved on to
# another replica), so it is discarded instead of being written over whatever is there now.
PENDING_MAX_AGE_SECONDS = 60.0
# Exiting with the platform unreachable must not hang for every pending task.
EXIT_FLUSH_BUDGET_SECONDS = 10.0
# A task id just looked up and not found reads as missing for this long without asking again.
# a2a-sdk 1.x looks a new task's id up several times before the task's first save, and that id is
# one the SDK generated for the request, so no other replica can have created the task meanwhile.
# A task that exists is always read from the server: another replica may have changed it.
ABSENT_CACHE_SECONDS = 2.0
# Bound on tasks whose write state is remembered. Forgetting an idle one costs its next write the
# version check: having no version to match, that write is unconditional.
MAX_TRACKED_TASKS = 4096
# The status a shell reports for a process ended by SIGTERM; kept when SIGTERM becomes an exit.
SIGTERM_EXIT_STATUS = 128 + signal.SIGTERM

# The order the registration contract spells its capability lists in.
CAPABILITY_ORDER = ("tasks", "kv")
# A startup registration that failed for a reason that may pass (no answer, 5xx, 429) is retried
# in the background after a delay that doubles from the first of these up to the second.
REGISTER_RETRY_FIRST_SECONDS = 1.0
REGISTER_RETRY_MAX_SECONDS = 60.0
REGISTER_THREAD_NAME = "nasiko-state-register"

T = TypeVar("T")


class NasikoStateError(Exception):
    """The state API refused a request or could not be reached.

    ``status`` and ``body`` carry the HTTP answer. Both are None for transport and
    configuration failures, where there was no answer.
    """

    def __init__(self, message: str, *, status: int | None = None, body: str | None = None):
        super().__init__(message)
        self.status = status
        self.body = body


class NasikoStateMalformedRow(NasikoStateError):
    """A stored row cannot be read back as what it claims to be.

    Raised for a task written by the other a2a-sdk major (or not by this module at all), and for a
    damaged record. Never answered with a half-decoded object.
    """


class NasikoStateTooLarge(NasikoStateError):
    """The state API refused a value above its size limit (HTTP 413). Retrying cannot help."""


class NasikoStateConflict(NasikoStateError):
    """An ``If-Match`` precondition failed: another writer changed the row first.

    ``current_version`` is the version the server holds, or None if the body did not say.
    """

    def __init__(self, message: str, *, current_version: int | None, body: str | None = None):
        super().__init__(message, status=412, body=body)
        self.current_version = current_version


@dataclass(frozen=True)
class WriteReceipt:
    """What the server answers to a PUT. Task and KV writes share the three fields."""

    version: int
    updated_at: str
    expires_at: str


@dataclass(frozen=True)
class TaskRecord:
    """One stored task. ``body`` is the SDK's serialized Task, opaque to the server."""

    task_id: str
    context_id: str | None
    state: str
    body: Any
    version: int
    updated_at: str
    expires_at: str


@dataclass(frozen=True)
class KVRecord:
    """One stored key. ``value`` is opaque JSON."""

    key: str
    value: Any
    version: int
    updated_at: str
    expires_at: str


@dataclass(frozen=True)
class Page(Generic[T]):
    """One page of a listing. ``next_cursor`` is None on the last page."""

    items: list[T]
    next_cursor: str | None


# --- Operations ---------------------------------------------------------------------------------
#
# Each state API call is described once, as data, and executed by the sync or the async client
# method. That keeps request building, status handling and parsing in a single place.

_UNSET: Any = object()


@dataclass(frozen=True)
class _Operation:
    """One state API call: how to send it and how to read the answer."""

    method: str
    path: str
    parse: Callable[[httpx.Response], Any]
    ok_status: int = 200
    params: Mapping[str, str | int | None] | None = None
    json: Any = _UNSET
    if_match: int | None = None
    # A write names the capability it needs registered; reads leave it None.
    capability: str | None = None
    # What a 404 means. Left unset, a 404 is an error like any other status.
    when_missing: Any = _UNSET
    # time.monotonic() by which the call, retries included, must be over. None means no deadline.
    deadline: float | None = None

    def request_kwargs(self) -> dict[str, Any]:
        kwargs: dict[str, Any] = {"method": self.method, "url": self.path}
        if self.params:
            kwargs["params"] = {key: value for key, value in self.params.items() if value is not None}
        if self.json is not _UNSET:
            kwargs["json"] = self.json
        if self.if_match is not None:
            kwargs["headers"] = {"If-Match": str(self.if_match)}
        if self.deadline is not None:
            kwargs["timeout"] = min(self.seconds_left(), READ_TIMEOUT_SECONDS)
        return kwargs

    def seconds_left(self) -> float:
        """Time until the deadline; infinite without one."""
        return float("inf") if self.deadline is None else self.deadline - time.monotonic()


def _percent_encode(value: str) -> str:
    """Escape everything but unreserved characters, so '/', ':' and '%' in an id stay inert.

    Used for URL path segments and for the components of composite keys alike.
    """
    return quote(value, safe="")


def _json_body(response: httpx.Response) -> Any:
    try:
        return response.json()
    except ValueError as exc:
        raise NasikoStateError(
            f"state API returned invalid JSON (HTTP {response.status_code})",
            status=response.status_code,
            body=response.text,
        ) from exc


def _record_parser(record_type: type[T]) -> Callable[[httpx.Response], T]:
    def parse(response: httpx.Response) -> T:
        return _record_from_json(record_type, _json_body(response))

    return parse


def _page_parser(record_type: type[T]) -> Callable[[httpx.Response], Page[T]]:
    def parse(response: httpx.Response) -> Page[T]:
        data = _json_body(response)
        try:
            items = [_record_from_json(record_type, item) for item in data["items"]]
            return Page(items=items, next_cursor=data.get("next_cursor"))
        except (KeyError, TypeError, AttributeError) as exc:
            raise NasikoStateError(f"unexpected listing payload from the state API: {data!r:.200}") from exc

    return parse


def _record_from_json(record_type: type[T], data: Any) -> T:
    try:
        values = {field.name: data[field.name] for field in fields(record_type)}
        return record_type(**values)  # type: ignore[call-arg]
    except (KeyError, TypeError) as exc:
        raise NasikoStateError(
            f"unexpected {record_type.__name__} payload from the state API: {data!r:.200}"
        ) from exc


def _register_op(capabilities: Sequence[str], client_name: str) -> _Operation:
    return _Operation(
        "POST",
        "/register",
        parse=lambda response: None,
        ok_status=204,
        json={"capabilities": list(capabilities), "client": client_name},
    )


def _put_task_op(
    task_id: str, fields: Mapping[str, Any], if_match: int | None, deadline: float | None
) -> _Operation:
    return _Operation(
        "PUT",
        f"/tasks/{_percent_encode(task_id)}",
        parse=_record_parser(WriteReceipt),
        json=dict(fields),
        if_match=if_match,
        capability="tasks",
        deadline=deadline,
    )


def _get_task_op(task_id: str) -> _Operation:
    return _Operation(
        "GET", f"/tasks/{_percent_encode(task_id)}", parse=_record_parser(TaskRecord), when_missing=None
    )


def _delete_task_op(task_id: str, if_match: int | None) -> _Operation:
    return _Operation(
        "DELETE",
        f"/tasks/{_percent_encode(task_id)}",
        parse=lambda response: True,
        ok_status=204,
        if_match=if_match,
        capability="tasks",
        when_missing=False,
    )


def _list_tasks_op(
    context_id: str | None, state: str | None, limit: int | None, cursor: str | None
) -> _Operation:
    return _Operation(
        "GET",
        "/tasks",
        parse=_page_parser(TaskRecord),
        params={"context_id": context_id, "state": state, "limit": limit, "cursor": cursor},
    )


def _put_kv_op(key: str, value: Any, if_match: int | None) -> _Operation:
    return _Operation(
        "PUT",
        f"/kv/{_percent_encode(key)}",
        parse=_record_parser(WriteReceipt),
        json={"value": value},
        if_match=if_match,
        capability="kv",
    )


def _get_kv_op(key: str) -> _Operation:
    return _Operation("GET", f"/kv/{_percent_encode(key)}", parse=_record_parser(KVRecord), when_missing=None)


def _delete_kv_op(key: str, if_match: int | None) -> _Operation:
    return _Operation(
        "DELETE",
        f"/kv/{_percent_encode(key)}",
        parse=lambda response: True,
        ok_status=204,
        if_match=if_match,
        capability="kv",
        when_missing=False,
    )


def _list_kv_op(prefix: str | None, limit: int | None, cursor: str | None) -> _Operation:
    return _Operation(
        "GET",
        "/kv",
        parse=_page_parser(KVRecord),
        params={"prefix": prefix, "limit": limit, "cursor": cursor},
    )


def _error_from(response: httpx.Response, operation: _Operation) -> NasikoStateError:
    body = response.text
    message = (
        f"state API {operation.method} {operation.path} failed with HTTP {response.status_code}: "
        f"{_problem_text(body)}"
    )
    if response.status_code == 412:
        return NasikoStateConflict(message, current_version=_current_version(body), body=body)
    if response.status_code == 413:
        return NasikoStateTooLarge(message, status=413, body=body)
    retry_after = response.headers.get("retry-after")
    if response.status_code == 429 and retry_after:
        message += f" (retry after {retry_after}s)"
    return NasikoStateError(message, status=response.status_code, body=body)


def _problem_text(body: str) -> str:
    """The server's own error text when the body is its JSON envelope, else the raw body."""
    try:
        data = json.loads(body)
    except ValueError:
        return body[:200]
    if isinstance(data, dict):
        return str(data.get("error") or data.get("message") or body)[:200]
    return body[:200]


def _current_version(body: str) -> int | None:
    try:
        version = json.loads(body).get("current_version")
    except (ValueError, AttributeError):
        return None
    return version if isinstance(version, int) else None


def _is_final_attempt(attempt: int) -> bool:
    return attempt >= RETRY_ATTEMPTS


# Failures after the request went out: the server may have applied it. Repeating a conditional
# write then conflicts with its own first attempt, so those are not retried.
_OUTCOME_UNKNOWN_ERRORS = (httpx.ReadTimeout, httpx.ReadError, httpx.RemoteProtocolError)


def _check_deadline(operation: _Operation) -> None:
    if operation.seconds_left() <= 0:
        raise NasikoStateError(
            f"state API {operation.method} {operation.path} ran out of time before it was sent"
        )


def _request_failed(operation: _Operation, exc: httpx.RequestError) -> NasikoStateError:
    return NasikoStateError(f"state API {operation.method} {operation.path} request failed: {exc!r}")


@dataclass
class _RetryState:
    """What one call has spent on retries so far."""

    # Attempts that failed in a way worth repeating (no answer, 5xx); bounded by RETRY_ATTEMPTS.
    transient_failures: int = 0
    # Seconds waited on 429 answers; bounded by RATE_LIMIT_BUDGET_SECONDS.
    rate_limited_seconds: float = 0.0


def _is_rate_limited(outcome: httpx.Response | httpx.RequestError) -> bool:
    return isinstance(outcome, httpx.Response) and outcome.status_code == 429


def _retry_after_seconds(response: httpx.Response) -> float:
    """How long a 429 asks to wait, within [RATE_LIMIT_MIN_WAIT_SECONDS, MAX_RETRY_AFTER_SECONDS]."""
    header = response.headers.get("retry-after")
    try:
        asked = float(header) if header is not None else RATE_LIMIT_DEFAULT_WAIT_SECONDS
    except ValueError:
        asked = RATE_LIMIT_DEFAULT_WAIT_SECONDS  # an HTTP-date; a per-minute window is not worth one
    return min(max(asked, RATE_LIMIT_MIN_WAIT_SECONDS), MAX_RETRY_AFTER_SECONDS)


def _jittered(seconds: float) -> float:
    """Spread a 429 wait so replicas limited together do not all come back at the same instant.

    Never shorter than the server asked, since an early retry is refused again and spends the
    agent's budget, except at the per-attempt cap, where the spread can only go down.
    """
    if seconds >= MAX_RETRY_AFTER_SECONDS:
        return MAX_RETRY_AFTER_SECONDS * random.uniform(1 - RATE_LIMIT_JITTER, 1)
    return min(MAX_RETRY_AFTER_SECONDS, seconds * random.uniform(1, 1 + RATE_LIMIT_JITTER))


def _rate_limit_delay(response: httpx.Response, retries: _RetryState) -> float | None:
    """The wait before repeating a rate-limited call, or None once the 429 budget is spent."""
    delay = _jittered(_retry_after_seconds(response))
    if retries.rate_limited_seconds + delay > RATE_LIMIT_BUDGET_SECONDS:
        return None
    retries.rate_limited_seconds += delay
    return delay


def _is_transient_outcome(operation: _Operation, outcome: httpx.Response | httpx.RequestError) -> bool:
    """Whether repeating the request could get a different answer: no answer at all, or a 5xx."""
    if isinstance(outcome, httpx.Response):
        return outcome.status_code >= 500
    # Only transport failures are transient; an undecodable answer or a redirect loop is not.
    if not isinstance(outcome, httpx.TransportError):
        return False
    return not (operation.if_match is not None and isinstance(outcome, _OUTCOME_UNKNOWN_ERRORS))


def _response_or_raise(operation: _Operation, outcome: httpx.Response | httpx.RequestError) -> httpx.Response:
    if isinstance(outcome, httpx.RequestError):
        raise _request_failed(operation, outcome) from outcome
    return outcome


def _is_transient(error: NasikoStateError) -> bool:
    """Whether the same request could succeed later: no answer at all, or a server-side failure."""
    return error.status is None or error.status >= 500


class NasikoStateClient:
    """Typed client for the platform's state API, with a sync and an async method per endpoint.

    Async methods carry an ``a`` prefix (``put_task`` / ``aput_task``), the way langgraph names
    its savers. Failures raise NasikoStateError with the HTTP status and body; nothing falls back
    silently. Connection errors and 5xx answers are retried (RETRY_ATTEMPTS attempts in total,
    exponential backoff) because every call is idempotent. A 429 is retried after the Retry-After
    it names, jittered and capped at MAX_RETRY_AFTER_SECONDS per attempt, until
    RATE_LIMIT_BUDGET_SECONDS of waiting is spent; then it raises like any other failure. Other
    4xx answers are never retried.

    The factories register the agent with the platform at startup (``register_in_background``),
    so a freshly deployed agent is known to keep durable state before it serves anything; failing
    that, the first write of each kind (tasks, kv) registers before it writes. Either way, once.
    """

    def __init__(
        self,
        base_url: str,
        token: str,
        *,
        client_name: str = CLIENT_NAME,
        transport: Any = None,
        retry_backoff_seconds: float = RETRY_BACKOFF_SECONDS,
    ) -> None:
        parts = urlsplit(base_url)
        if parts.scheme not in ("http", "https") or not parts.netloc:
            raise NasikoStateError(f"{STATE_URL_ENV} must be an http(s) URL, got {base_url!r}")
        if not token:
            raise NasikoStateError("the state API needs an agent token and none was given")
        self._base_url = base_url.rstrip("/")
        self._headers = {"Authorization": f"Bearer {token}", "User-Agent": client_name}
        self._client_name = client_name
        self._transport = transport
        self._retry_backoff = retry_backoff_seconds
        self._sync_client: httpx.Client | None = None
        # Sync calls come from several threads (a sync langgraph run, the exit flush).
        self._sync_client_lock = threading.Lock()
        self._async_client: httpx.AsyncClient | None = None
        self._registered: set[str] = set()
        self._register_lock = threading.Lock()
        self._async_register_lock = asyncio.Lock()

    @classmethod
    def from_env(cls, env: Mapping[str, str] | None = None) -> NasikoStateClient | None:
        """Build a client from the platform's environment, or None when it provides no state API."""
        env = os.environ if env is None else env
        url = env.get(STATE_URL_ENV, "").strip()
        if not url:
            return None
        token = env.get(AGENT_TOKEN_ENV, "").strip()
        if not token:
            raise NasikoStateError(
                f"{STATE_URL_ENV} is set but {AGENT_TOKEN_ENV} is empty or missing: the state API needs "
                f"this agent's state credential, which the platform injects as {AGENT_TOKEN_ENV}"
            )
        return cls(url, token)

    def __repr__(self) -> str:
        return f"NasikoStateClient({self._base_url!r})"

    # --- registration ----------------------------------------------------------------------

    def register(self, capabilities: Sequence[str], client_name: str | None = None) -> None:
        """Tell the platform which kinds of state this agent keeps. Writes do this on their own."""
        self._execute(_register_op(capabilities, client_name or self._client_name))
        self._registered.update(capabilities)

    async def aregister(self, capabilities: Sequence[str], client_name: str | None = None) -> None:
        await self._aexecute(_register_op(capabilities, client_name or self._client_name))
        self._registered.update(capabilities)

    def register_in_background(self, capability: str) -> threading.Thread:
        """Register ``capability`` from a daemon thread, so starting the agent never waits on it.

        A failure that may pass is retried on a capped backoff until it succeeds. A refusal is
        logged and left to the first write, which registers before it writes.
        """
        thread = threading.Thread(
            target=self._register_until_done, args=(capability,), name=REGISTER_THREAD_NAME, daemon=True
        )
        thread.start()
        return thread

    def _register_until_done(self, capability: str) -> None:
        delay = REGISTER_RETRY_FIRST_SECONDS
        while True:
            try:
                self._ensure_registered(capability)
                return
            except NasikoStateError as exc:
                if not (_is_transient(exc) or exc.status == 429):
                    logger.error(
                        "registering %s state with the platform failed; the first write will try again: %s",
                        capability,
                        exc,
                    )
                    return
                logger.warning(
                    "registering %s state with the platform failed, retrying in %.0f s: %s",
                    capability,
                    delay,
                    exc,
                )
            time.sleep(delay)
            delay = min(delay * 2, REGISTER_RETRY_MAX_SECONDS)

    # --- tasks -----------------------------------------------------------------------------

    def put_task(
        self,
        task_id: str,
        *,
        context_id: str | None,
        state: str,
        body: Any,
        if_match: int | None = None,
        deadline: float | None = None,
    ) -> WriteReceipt:
        """Create or replace a task. ``deadline`` (time.monotonic()) bounds the call, retries included."""
        fields = {"context_id": context_id, "state": state, "body": body}
        return self._run(_put_task_op(task_id, fields, if_match, deadline))

    async def aput_task(
        self,
        task_id: str,
        *,
        context_id: str | None,
        state: str,
        body: Any,
        if_match: int | None = None,
        deadline: float | None = None,
    ) -> WriteReceipt:
        fields = {"context_id": context_id, "state": state, "body": body}
        return await self._arun(_put_task_op(task_id, fields, if_match, deadline))

    def get_task(self, task_id: str) -> TaskRecord | None:
        return self._run(_get_task_op(task_id))

    async def aget_task(self, task_id: str) -> TaskRecord | None:
        return await self._arun(_get_task_op(task_id))

    def delete_task(self, task_id: str, *, if_match: int | None = None) -> bool:
        """Delete a task. False means there was no such task."""
        return self._run(_delete_task_op(task_id, if_match))

    async def adelete_task(self, task_id: str, *, if_match: int | None = None) -> bool:
        return await self._arun(_delete_task_op(task_id, if_match))

    def list_tasks(
        self,
        *,
        context_id: str | None = None,
        state: str | None = None,
        limit: int | None = None,
        cursor: str | None = None,
    ) -> Page[TaskRecord]:
        return self._run(_list_tasks_op(context_id, state, limit, cursor))

    async def alist_tasks(
        self,
        *,
        context_id: str | None = None,
        state: str | None = None,
        limit: int | None = None,
        cursor: str | None = None,
    ) -> Page[TaskRecord]:
        return await self._arun(_list_tasks_op(context_id, state, limit, cursor))

    # --- key-value -------------------------------------------------------------------------

    def put_kv(self, key: str, value: Any, *, if_match: int | None = None) -> WriteReceipt:
        return self._run(_put_kv_op(key, value, if_match))

    async def aput_kv(self, key: str, value: Any, *, if_match: int | None = None) -> WriteReceipt:
        return await self._arun(_put_kv_op(key, value, if_match))

    def get_kv(self, key: str) -> KVRecord | None:
        return self._run(_get_kv_op(key))

    async def aget_kv(self, key: str) -> KVRecord | None:
        return await self._arun(_get_kv_op(key))

    def delete_kv(self, key: str, *, if_match: int | None = None) -> bool:
        """Delete a key. False means there was no such key."""
        return self._run(_delete_kv_op(key, if_match))

    async def adelete_kv(self, key: str, *, if_match: int | None = None) -> bool:
        return await self._arun(_delete_kv_op(key, if_match))

    def list_kv(
        self, *, prefix: str | None = None, limit: int | None = None, cursor: str | None = None
    ) -> Page[KVRecord]:
        return self._run(_list_kv_op(prefix, limit, cursor))

    async def alist_kv(
        self, *, prefix: str | None = None, limit: int | None = None, cursor: str | None = None
    ) -> Page[KVRecord]:
        return await self._arun(_list_kv_op(prefix, limit, cursor))

    # --- lifecycle -------------------------------------------------------------------------

    def close(self) -> None:
        with self._sync_client_lock:
            if self._sync_client is not None:
                self._sync_client.close()
                self._sync_client = None

    async def aclose(self) -> None:
        if self._async_client is not None:
            await self._async_client.aclose()
            self._async_client = None
        self.close()

    # --- execution -------------------------------------------------------------------------

    def _run(self, operation: _Operation) -> Any:
        if operation.capability is not None:
            self._ensure_registered(operation.capability)
        return self._execute(operation)

    async def _arun(self, operation: _Operation) -> Any:
        if operation.capability is not None:
            await self._aensure_registered(operation.capability)
        return await self._aexecute(operation)

    def _execute(self, operation: _Operation) -> Any:
        return self._interpret(operation, self._send(operation))

    async def _aexecute(self, operation: _Operation) -> Any:
        return self._interpret(operation, await self._asend(operation))

    def _interpret(self, operation: _Operation, response: httpx.Response) -> Any:
        if response.status_code == 404 and operation.when_missing is not _UNSET:
            return operation.when_missing
        if response.status_code != operation.ok_status:
            raise _error_from(response, operation)
        return operation.parse(response)

    def _send(self, operation: _Operation) -> httpx.Response:
        http = self._sync_http()
        retries = _RetryState()
        while True:
            _check_deadline(operation)
            try:
                outcome: httpx.Response | httpx.RequestError = http.request(**operation.request_kwargs())
            except httpx.RequestError as exc:
                outcome = exc
            delay = self._retry_delay(operation, retries, outcome)
            if delay is None:
                return _response_or_raise(operation, outcome)
            time.sleep(delay)

    async def _asend(self, operation: _Operation) -> httpx.Response:
        http = self._async_http()
        retries = _RetryState()
        while True:
            _check_deadline(operation)
            try:
                outcome: httpx.Response | httpx.RequestError = await http.request(
                    **operation.request_kwargs()
                )
            except httpx.RequestError as exc:
                outcome = exc
            delay = self._retry_delay(operation, retries, outcome)
            if delay is None:
                return _response_or_raise(operation, outcome)
            await asyncio.sleep(delay)

    def _retry_delay(
        self, operation: _Operation, retries: _RetryState, outcome: httpx.Response | httpx.RequestError
    ) -> float | None:
        """Seconds to wait before trying again, or None when this outcome is the answer.

        Records the wait in ``retries``, so it is only to be called once per outcome.
        """
        if _is_rate_limited(outcome):
            delay = _rate_limit_delay(outcome, retries)  # type: ignore[arg-type]
        else:
            delay = self._transient_delay(operation, retries, outcome)
        if delay is None or delay >= operation.seconds_left():
            return None
        return delay

    def _transient_delay(
        self, operation: _Operation, retries: _RetryState, outcome: httpx.Response | httpx.RequestError
    ) -> float | None:
        if not _is_transient_outcome(operation, outcome):
            return None
        retries.transient_failures += 1
        if _is_final_attempt(retries.transient_failures):
            return None
        return self._backoff(retries.transient_failures)

    def _backoff(self, attempt: int) -> float:
        return self._retry_backoff * 2 ** (attempt - 1)

    def _ensure_registered(self, capability: str) -> None:
        if capability in self._registered:
            return
        with self._register_lock:
            if capability in self._registered:
                return
            self.register(self._capabilities_with(capability))

    async def _aensure_registered(self, capability: str) -> None:
        if capability in self._registered:
            return
        async with self._async_register_lock:
            if capability in self._registered:
                return
            await self.aregister(self._capabilities_with(capability))

    def _capabilities_with(self, capability: str) -> list[str]:
        """Everything registered so far plus this capability, in the contract's order."""
        wanted = self._registered | {capability}
        known = [name for name in CAPABILITY_ORDER if name in wanted]
        return known + sorted(wanted.difference(CAPABILITY_ORDER))

    def _sync_http(self) -> httpx.Client:
        with self._sync_client_lock:
            if self._sync_client is None:
                self._sync_client = httpx.Client(
                    base_url=self._base_url,
                    headers=self._headers,
                    timeout=_timeout(),
                    transport=self._transport,
                )
            return self._sync_client

    def _async_http(self) -> httpx.AsyncClient:
        if self._async_client is None:
            self._async_client = httpx.AsyncClient(
                base_url=self._base_url,
                headers=self._headers,
                timeout=_timeout(),
                transport=self._transport,
            )
        return self._async_client


def _timeout() -> httpx.Timeout:
    return httpx.Timeout(
        connect=CONNECT_TIMEOUT_SECONDS,
        read=READ_TIMEOUT_SECONDS,
        write=READ_TIMEOUT_SECONDS,
        pool=CONNECT_TIMEOUT_SECONDS,
    )


# --- a2a-sdk major-version adapter --------------------------------------------------------------
#
# 0.3.x tasks are pydantic models; 1.x tasks are protobuf messages with no model_dump. Everything
# the store needs from a task goes through these names, so the store itself is version-blind.
#
# The two majors serialize a task differently, and a body written by one can parse under the other
# into a task with silently wrong fields (protobuf drops what it does not know). Every stored body
# is therefore tagged with the format that wrote it, and a foreign tag is refused outright.

TASK_FORMAT_A2A_0_3 = "a2a-0.3"
TASK_FORMAT_A2A_1 = "a2a-1"

# The state column stored with each task uses one vocabulary under both SDK majors, the A2A 0.3
# wire spelling: submitted, working, input-required, auth-required, completed, canceled, failed,
# rejected, unknown. 1.x names (TASK_STATE_INPUT_REQUIRED) are mapped onto it.

if _a2a_pb2 is None:
    TASK_FORMAT = TASK_FORMAT_A2A_0_3
    _DECODE_ERRORS: tuple[type[BaseException], ...] = (ValueError, TypeError)

    def _encode_task(task: Task) -> dict[str, Any]:
        return task.model_dump(mode="json", exclude_none=True)

    def _decode_task(body: Any) -> Task:
        return Task.model_validate(body)

    def _state_name(task: Task) -> str:
        return task.status.state.value

else:
    from google.protobuf.json_format import MessageToDict, ParseDict, ParseError

    TASK_FORMAT = TASK_FORMAT_A2A_1
    _DECODE_ERRORS = (ParseError, ValueError, TypeError, AttributeError)

    def _encode_task(task: Task) -> dict[str, Any]:
        return MessageToDict(task)

    def _decode_task(body: Any) -> Task:
        # Strict: an unknown field means the body is not what its tag claims, and protobuf would
        # otherwise drop it (and any unknown enum value) without a word.
        return ParseDict(body, Task())

    def _state_name(task: Task) -> str:
        return _proto_state_name(task.status.state)

    def _proto_state_name(state: int) -> str:
        """'TASK_STATE_INPUT_REQUIRED' -> 'input-required', the 0.3 wire spelling."""
        name = _a2a_pb2.TaskState.Name(state).removeprefix("TASK_STATE_").lower().replace("_", "-")
        return "unknown" if name == "unspecified" else name

    def _list_arguments(params: Any) -> dict[str, Any]:
        """Map a ListTasksRequest onto the state API's listing filters."""
        return {
            "context_id": params.context_id or None,
            "state": _proto_state_name(params.status) if params.status else None,
            "limit": params.page_size or DEFAULT_PAGE_SIZE,
            "cursor": params.page_token or None,
        }

    def _status_changed_since(task: Task, params: Any) -> bool:
        """tasks/list's status_timestamp_after, applied here: the state API cannot filter on it."""
        if not params.HasField("status_timestamp_after"):
            return True
        if not task.status.HasField("timestamp"):
            return False
        after, stamp = params.status_timestamp_after, task.status.timestamp
        return (stamp.seconds, stamp.nanos) >= (after.seconds, after.nanos)

    def _list_response(tasks: list[Task], page: Page[TaskRecord], limit: int) -> Any:
        # total_size stays unset: the state API reports no totals, and this page's own size would
        # be a wrong one. A page the timestamp filter thinned out is short, and its token still
        # leads to the next.
        return _a2a_pb2.ListTasksResponse(
            tasks=tasks, next_page_token=page.next_cursor or "", page_size=limit
        )


def _stored_body(task: Task) -> dict[str, Any]:
    """The body written to the state API: the serialized task, tagged with its format."""
    return {"format": TASK_FORMAT, "task": _encode_task(task)}


def _decode_stored_task(body: Any, task_id: str) -> Task:
    stored_format = body.get("format") if isinstance(body, dict) else None
    if stored_format != TASK_FORMAT or "task" not in body:
        raise NasikoStateMalformedRow(
            f"stored task {task_id!r} is in format {stored_format!r}, and the installed a2a-sdk reads "
            f"{TASK_FORMAT!r} only"
        )
    try:
        task = _decode_task(body["task"])
    except _DECODE_ERRORS as exc:
        raise NasikoStateMalformedRow(
            f"stored task {task_id!r} cannot be decoded by the installed a2a-sdk: {exc}"
        ) from exc
    # protobuf parsing turns {} into an empty Task rather than failing, so check the identity too.
    if task.id != task_id:
        raise NasikoStateMalformedRow(f"stored task {task_id!r} holds a body for task {task.id!r}")
    return task


# --- task store ---------------------------------------------------------------------------------


@dataclass(frozen=True)
class _Snapshot:
    """A task as it stood when save() was called.

    The SDK mutates its Task in place after saving it, so the store keeps this serialized copy
    and never the live object.
    """

    task_id: str
    context_id: str | None
    state: str
    body: dict[str, Any]
    saved_at: float  # time.monotonic() at save()

    def is_stale(self) -> bool:
        return time.monotonic() - self.saved_at > PENDING_MAX_AGE_SECONDS


def _snapshot(task: Task) -> _Snapshot:
    return _Snapshot(
        task.id, task.context_id or None, _state_name(task), _stored_body(task), time.monotonic()
    )


class _TaskSlot:
    """Write-coalescing and concurrency bookkeeping for one task."""

    def __init__(self, task_id: str) -> None:
        self.task_id = task_id
        # The newest snapshot the server has not confirmed yet.
        self.pending: _Snapshot | None = None
        # The row's version as this process last saw it, from a confirmed write or a read. Every
        # write is conditional on it (If-Match). None until this process has written or read it.
        self.version: int | None = None
        # The state of the newest snapshot sent to the server, or read from it. A save in any other
        # state is written through. None when unknown: nothing sent yet, or the last send failed.
        self.state: str | None = None
        self.last_write_started = float("-inf")
        # time.monotonic() until which a lookup that found nothing stands; see ABSENT_CACHE_SECONDS.
        self.absent_until = float("-inf")
        # Consecutive failed writes; spaces out the background retries.
        self.failed_writes = 0
        self.flusher: asyncio.Task[None] | None = None
        # One write per task at a time, so an older snapshot cannot land after a newer one.
        self.lock = asyncio.Lock()

    def is_idle(self) -> bool:
        return self.pending is None and self.flusher is None and not self.lock.locked()

    def is_known_absent(self) -> bool:
        """Whether a recent lookup found no such task and nothing has been saved since."""
        return self.pending is None and self.version is None and time.monotonic() < self.absent_until


class NasikoTaskStore(TaskStore):
    """An a2a-sdk TaskStore that keeps tasks in the platform's state API.

    Writes, by what changed:
      - A save that changes ``task.status.state`` (the first save of a task, input-required,
        every terminal state) is on the server before ``save()`` returns.
      - Any other save (artifact chunks, message history) is coalesced: the newest snapshot is
        written at most once per ``coalesce_window_seconds`` per task, and always flushed when the
        task changes state and on ``aclose()``. A write that fails is retried in the background on
        a growing delay; an update still unwritten after PENDING_MAX_AGE_SECONDS is discarded.
      - Guarantee boundary: a hard crash (SIGKILL, power loss) can lose at most one window,
        0.5 s by default, of non-status updates. State changes are never lost once save() returned.

    Replicas. Every write is conditional on the version this process last saw of the task, so a
    replica can never overwrite a newer write it has not seen. When another replica got there
    first, a save that changes state raises NasikoStateConflict, and a coalesced update is dropped
    (logged); either way the store re-reads the task, and ``get()`` returns the other replica's
    row. ``get()`` returns this process's unwritten snapshot only while it is still based on the
    server's current row; a task it did not find reads as missing for ABSENT_CACHE_SECONDS
    without another request. ``list`` is served by the server and can lag by one window.

    Tasks are scoped to the agent, not to an end user: the SDK's per-user owner resolution does
    not apply.
    """

    def __init__(
        self, client: NasikoStateClient, *, coalesce_window_seconds: float = COALESCE_WINDOW_SECONDS
    ) -> None:
        self._client = client
        self._window = coalesce_window_seconds
        self._slots: OrderedDict[str, _TaskSlot] = OrderedDict()
        atexit.register(self.flush_sync)
        _open_task_stores.add(self)

    async def save(self, task: Task, context: Any = None) -> None:
        snapshot = _snapshot(task)
        slot = self._slot(snapshot.task_id)
        slot.pending = snapshot
        slot.absent_until = float("-inf")
        if snapshot.state == slot.state:
            self._schedule_flush(slot)
            return
        slot.state = snapshot.state
        try:
            await self._write_pending(slot)
        except NasikoStateError as exc:
            if _is_transient(exc):
                # The state change is still owed; retry it rather than wait for a save that may
                # never come.
                self._schedule_flush(slot)
            raise

    async def get(self, task_id: str, context: Any = None) -> Task | None:
        slot = self._slot(task_id)
        # Under the slot's lock, so a write in flight cannot be mistaken for another writer's.
        async with slot.lock:
            if slot.is_known_absent():
                return None
            record = await self._client.aget_task(task_id)
            task = self._newest_of(slot, record)
            if task is None:
                slot.absent_until = time.monotonic() + ABSENT_CACHE_SECONDS
            return task

    async def delete(self, task_id: str, context: Any = None) -> None:
        slot = self._slot(task_id)
        async with slot.lock:
            slot.pending = None  # an unwritten snapshot must not bring the task back
            await self._client.adelete_task(task_id)
        self._forget(task_id, slot)

    if _a2a_pb2 is not None:
        # a2a-sdk 1.x added tasks/list to the TaskStore interface; 0.3.x has no such method.

        async def list(self, params: Any, context: Any = None) -> Any:
            arguments = _list_arguments(params)
            page = await self._client.alist_tasks(**arguments)
            tasks = [_decode_stored_task(record.body, record.task_id) for record in page.items]
            recent = [task for task in tasks if _status_changed_since(task, params)]
            return _list_response(recent, page, arguments["limit"])

    async def aclose(self) -> None:
        """Write everything pending, stop the background flushers and close the HTTP client."""
        atexit.unregister(self.flush_sync)
        _open_task_stores.discard(self)
        flushers = [slot.flusher for slot in self._slots.values() if slot.flusher is not None]
        for flusher in flushers:
            flusher.cancel()
        await asyncio.gather(*flushers, return_exceptions=True)
        owed = [slot for slot in list(self._slots.values()) if self._owes_a_write(slot)]
        outcomes = await asyncio.gather(*(self._write_pending(slot) for slot in owed), return_exceptions=True)
        await self._client.aclose()
        failures = []
        for slot, outcome in zip(owed, outcomes):
            if isinstance(outcome, NasikoStateConflict):
                _log_superseded(slot, outcome)
            elif isinstance(outcome, BaseException):
                failures.append(outcome)
        for failure in failures[1:]:
            logger.error("a pending task write could not be flushed on close: %s", failure)
        if failures:
            raise failures[0]

    def flush_sync(self) -> None:
        """Write every pending snapshot, blocking. Registered with atexit, when no event loop is left.

        EXIT_FLUSH_BUDGET_SECONDS bounds the whole flush, each request and its retries included, so
        an unreachable platform cannot hold up the exit.
        """
        deadline = time.monotonic() + EXIT_FLUSH_BUDGET_SECONDS
        for slot in list(self._slots.values()):
            if not self._owes_a_write(slot):
                continue
            if time.monotonic() >= deadline:
                logger.error(
                    "exit flush budget spent; task %s and any after it were not written", slot.task_id
                )
                return
            self._write_pending_sync(slot, deadline)

    # --- writing ---------------------------------------------------------------------------

    async def _write_pending(self, slot: _TaskSlot) -> None:
        async with slot.lock:
            snapshot = slot.pending
            if snapshot is None:
                return  # a newer write already carried it
            slot.last_write_started = time.monotonic()
            try:
                receipt = await self._client.aput_task(
                    snapshot.task_id,
                    context_id=snapshot.context_id,
                    state=snapshot.state,
                    body=snapshot.body,
                    if_match=slot.version,
                )
            except NasikoStateConflict:
                await self._reread_after_conflict(slot)
                raise
            except NasikoStateError as exc:
                self._record_failure(slot, exc)
                raise
            self._confirm(slot, snapshot, receipt.version)

    def _write_pending_sync(self, slot: _TaskSlot, deadline: float) -> None:
        """The exit-time twin of _write_pending: no event loop, so no lock and no re-read."""
        snapshot = slot.pending
        assert snapshot is not None, "callers check _owes_a_write first"
        try:
            receipt = self._client.put_task(
                snapshot.task_id,
                context_id=snapshot.context_id,
                state=snapshot.state,
                body=snapshot.body,
                if_match=slot.version,
                deadline=deadline,
            )
        except NasikoStateConflict as exc:
            slot.pending = None
            _log_superseded(slot, exc)
        except NasikoStateError as exc:
            logger.error("exit flush of task %s failed: %s", slot.task_id, exc)
        else:
            self._confirm(slot, snapshot, receipt.version)

    def _confirm(self, slot: _TaskSlot, snapshot: _Snapshot, version: int) -> None:
        slot.version = version
        slot.failed_writes = 0
        if slot.state is None:
            slot.state = snapshot.state
        if slot.pending is snapshot:
            slot.pending = None
        # Otherwise a newer snapshot arrived while this one was in flight; it is still owed.

    def _record_failure(self, slot: _TaskSlot, error: NasikoStateError) -> None:
        slot.failed_writes += 1
        slot.state = None  # unknown whether the server has it, so the next save writes through
        if not _is_transient(error):
            slot.pending = None  # retrying cannot help; the caller or the log has the error

    async def _reread_after_conflict(self, slot: _TaskSlot) -> None:
        """Another writer changed the task first: drop what this process had not written, re-read."""
        slot.pending = None
        slot.state = None
        try:
            record = await self._client.aget_task(slot.task_id)
        except NasikoStateError as exc:
            # The stale version is kept, so the next write conflicts again and retries the read.
            logger.error("re-reading task %s after a write conflict failed: %s", slot.task_id, exc)
            return
        self._adopt(slot, record)

    # --- reading ---------------------------------------------------------------------------

    def _newest_of(self, slot: _TaskSlot, record: TaskRecord | None) -> Task | None:
        """This process's unwritten snapshot while it is based on the server's row, else the row."""
        if slot.pending is not None and (record is None or record.version == slot.version):
            return _decode_stored_task(slot.pending.body, slot.task_id)
        if record is None:
            return None
        task = _decode_stored_task(record.body, slot.task_id)
        if slot.pending is not None:
            logger.warning(
                "task %s was changed by another writer (version %s); this replica's unwritten "
                "update was dropped",
                slot.task_id,
                record.version,
            )
            slot.pending = None
        self._adopt(slot, record)
        return task

    def _adopt(self, slot: _TaskSlot, record: TaskRecord | None) -> None:
        slot.version = None if record is None else record.version
        slot.state = None if record is None else record.state

    # --- coalescing ------------------------------------------------------------------------

    def _schedule_flush(self, slot: _TaskSlot) -> None:
        if slot.flusher is None:
            slot.flusher = asyncio.get_running_loop().create_task(self._flush_when_due(slot))

    async def _flush_when_due(self, slot: _TaskSlot) -> None:
        try:
            while self._owes_a_write(slot):
                await asyncio.sleep(self._seconds_until_due(slot))
                if not self._owes_a_write(slot):
                    return
                await self._try_deferred_write(slot)
        finally:
            slot.flusher = None

    async def _try_deferred_write(self, slot: _TaskSlot) -> None:
        # No caller awaits a deferred write to hand its error to, so each outcome is logged here.
        try:
            await self._write_pending(slot)
        except NasikoStateConflict as exc:
            _log_superseded(slot, exc)
        except NasikoStateError as exc:
            retry = "will retry" if slot.pending is not None else "dropped"
            logger.error("deferred write of task %s failed (%s): %s", slot.task_id, retry, exc)

    def _seconds_until_due(self, slot: _TaskSlot) -> float:
        delay = min(self._window * 2**slot.failed_writes, max(self._window, DEFERRED_RETRY_MAX_SECONDS))
        return max(0.0, slot.last_write_started + delay - time.monotonic())

    def _owes_a_write(self, slot: _TaskSlot) -> bool:
        """Whether the slot has an update to write, discarding it instead when it is stale."""
        snapshot = slot.pending
        if snapshot is None:
            return False
        if not snapshot.is_stale():
            return True
        logger.error(
            "task %s: discarding an update that could not be written for over %.0f s",
            slot.task_id,
            PENDING_MAX_AGE_SECONDS,
        )
        slot.pending = None
        return False

    # --- slot bookkeeping ------------------------------------------------------------------

    def _slot(self, task_id: str) -> _TaskSlot:
        slot = self._slots.get(task_id)
        if slot is not None:
            self._slots.move_to_end(task_id)
            return slot
        slot = self._slots[task_id] = _TaskSlot(task_id)
        self._evict_idle_slots()
        return slot

    def _forget(self, task_id: str, slot: _TaskSlot) -> None:
        if self._slots.get(task_id) is slot:
            del self._slots[task_id]

    def _evict_idle_slots(self) -> None:
        """Keep memory bounded: drop the least recently used slots that owe the server nothing.

        The newest slot is the caller's and is never a candidate.
        """
        excess = len(self._slots) - MAX_TRACKED_TASKS
        for task_id in list(self._slots)[:-1]:
            if excess <= 0:
                return
            if self._slots[task_id].is_idle():
                del self._slots[task_id]
                excess -= 1


def _log_superseded(slot: _TaskSlot, conflict: NasikoStateConflict) -> None:
    logger.warning(
        "task %s was changed by another writer (now version %s); this replica's unwritten update was dropped",
        slot.task_id,
        conflict.current_version,
    )


# Durable task stores this process opened and has not closed: what install_shutdown_hooks closes.
_open_task_stores: set[NasikoTaskStore] = set()


def make_task_store() -> TaskStore:
    """The task store an agent hands to its request handler.

    The platform's durable store when NASIKO_STATE_URL is set, otherwise the SDK's in-memory
    store, built exactly as an agent would build it by hand. The durable store registers the agent
    with the platform in the background right away, so the agent counts as durable before its
    first request; the in-memory one never contacts the platform.
    """
    client = NasikoStateClient.from_env()
    if client is None:
        return InMemoryTaskStore()
    logger.info("task state is kept in the platform state API: %r", client)
    store = NasikoTaskStore(client)
    client.register_in_background("tasks")
    return store


def install_shutdown_hooks(app: Any) -> None:
    """Flush durable task state when the server stops, instead of losing what is still pending.

    Call it once on the Starlette application the agent serves, before wrapping it in any plain
    ASGI middleware. It closes every NasikoTaskStore this process opened, which writes their
    pending updates:

      - from the application's lifespan shutdown, which uvicorn runs on SIGTERM and Ctrl-C;
      - as a fallback when nothing else handles SIGTERM, by turning SIGTERM into a normal exit, so
        the atexit flush runs instead of the process dying with writes pending.

    Without NASIKO_STATE_URL the agent has no durable store, and both hooks find nothing to do.
    """
    router = getattr(app, "router", None)
    if not hasattr(router, "lifespan_context"):
        raise TypeError(
            f"install_shutdown_hooks needs the Starlette application itself, got {type(app).__name__}; "
            "call it before wrapping the application in middleware"
        )
    router.lifespan_context = _closing_task_stores(router.lifespan_context)
    _exit_normally_on_sigterm()


def _closing_task_stores(lifespan_context: Callable[[Any], Any]) -> Callable[[Any], Any]:
    """The application's own lifespan, followed by closing the task stores on shutdown."""

    @contextlib.asynccontextmanager
    async def lifespan(app: Any) -> AsyncIterator[Any]:
        try:
            async with lifespan_context(app) as state:
                yield state
        finally:
            await _close_open_task_stores()

    return lifespan


async def _close_open_task_stores() -> None:
    for store in list(_open_task_stores):
        try:
            await store.aclose()
        except NasikoStateError as exc:
            # Raising would only turn into a failed-shutdown log line from the server; say what failed.
            logger.error("task state could not be flushed at shutdown: %s", exc)


def _exit_normally_on_sigterm() -> None:
    # Python's default SIGTERM action ends the process on the spot, skipping atexit and with it
    # the last flush. uvicorn replaces any handler while it serves and re-raises the signal after
    # its own shutdown, so this one only acts when nothing else did. A handler someone else
    # installed is left alone.
    if signal.getsignal(signal.SIGTERM) is signal.SIG_DFL:
        signal.signal(signal.SIGTERM, _raise_system_exit)


def _raise_system_exit(signum: int, frame: Any) -> None:
    raise SystemExit(SIGTERM_EXIT_STATUS)


# --- key-value ----------------------------------------------------------------------------------


class NasikoKV:
    """Key-value access to the platform's state API, scoped to this agent. Values are opaque JSON.

    ``list`` follows the server's cursors, so callers see every key under a prefix. Like the client
    it wraps, each operation has a sync form and an ``a``-prefixed async form.
    """

    def __init__(self, client: NasikoStateClient) -> None:
        self._client = client

    def get(self, key: str) -> KVRecord | None:
        return self._client.get_kv(key)

    async def aget(self, key: str) -> KVRecord | None:
        return await self._client.aget_kv(key)

    def put(self, key: str, value: Any, *, if_match: int | None = None) -> WriteReceipt:
        return self._client.put_kv(key, value, if_match=if_match)

    async def aput(self, key: str, value: Any, *, if_match: int | None = None) -> WriteReceipt:
        return await self._client.aput_kv(key, value, if_match=if_match)

    def delete(self, key: str, *, if_match: int | None = None) -> bool:
        """Delete a key. False means there was no such key."""
        return self._client.delete_kv(key, if_match=if_match)

    async def adelete(self, key: str, *, if_match: int | None = None) -> bool:
        return await self._client.adelete_kv(key, if_match=if_match)

    def list(self, prefix: str = "") -> Iterator[KVRecord]:
        """Every record whose key starts with ``prefix``, in the server's order."""
        cursor: str | None = None
        while True:
            page = self._client.list_kv(prefix=prefix or None, limit=MAX_PAGE_SIZE, cursor=cursor)
            yield from page.items
            if page.next_cursor is None:
                return
            cursor = page.next_cursor

    async def alist(self, prefix: str = "") -> AsyncIterator[KVRecord]:
        cursor: str | None = None
        while True:
            page = await self._client.alist_kv(prefix=prefix or None, limit=MAX_PAGE_SIZE, cursor=cursor)
            for record in page.items:
                yield record
            if page.next_cursor is None:
                return
            cursor = page.next_cursor


# --- langgraph checkpointer ---------------------------------------------------------------------


def _reads_stored_records(function: Callable[..., T]) -> Callable[..., T]:
    """Report a damaged stored record as NasikoStateMalformedRow, not as a bare KeyError.

    Only for helpers that parse records already fetched: they make no requests, so every one of
    these exceptions comes from the record's shape or encoding.
    """

    @functools.wraps(function)
    def parse(*args: Any, **kwargs: Any) -> T:
        try:
            return function(*args, **kwargs)
        except (KeyError, TypeError, ValueError) as exc:
            raise NasikoStateMalformedRow(f"a stored checkpoint record is malformed: {exc!r}") from exc

    return parse


# A checkpoint record carries its channel values inline while it stays within this many bytes, so
# a put is two writes (the record and the head) and reading the newest checkpoint is one read plus
# the writes listing. A bigger state, or one the platform refuses as too large (its limit is a
# server setting this module cannot see), spills every non-scalar value into its own blob record.
INLINE_CHECKPOINT_MAX_BYTES = 256 * 1024

CHECKPOINT_FAMILY = "lg"
HEAD_FAMILY = "lgh"
WRITES_FAMILY = "lgw"
BLOB_FAMILY = "lgb"


def _key(family: str, *parts: str) -> str:
    return ":".join([family, *(_percent_encode(part) for part in parts)])


def _prefix(family: str, *parts: str) -> str:
    """Prefix matching every key of `family` that starts with these leading components."""
    return _key(family, *parts) + ":"


def _thread_and_namespace(config: Mapping[str, Any]) -> tuple[str, str]:
    configurable = config["configurable"]
    return configurable["thread_id"], configurable.get("checkpoint_ns", "")


def _scope(config: Mapping[str, Any] | None) -> tuple[str, ...]:
    """The leading key components a listing is narrowed to: nothing, a thread, or a thread and namespace."""
    if not config:
        return ()
    configurable = config["configurable"]
    if configurable.get("checkpoint_ns") is None:
        return (configurable["thread_id"],)
    return (configurable["thread_id"], configurable["checkpoint_ns"])


class NasikoCheckpointSaver(BaseCheckpointSaver):  # type: ignore[misc]
    """A langgraph checkpointer that keeps checkpoints in the platform's key-value state.

    It implements the sync methods (``graph.stream`` and ``graph.invoke``) and their async twins
    (``astream``, ``ainvoke``). The optional ``copy_thread``, ``prune`` and ``delete_for_runs``
    are left to the base class, which refuses them.

    Layout. Every component is percent-encoded, so ':' only ever separates components, and every
    langgraph object is written by the saver's own serde and carried as a base64 string in a JSON
    record. Pickle is never written or read.

      lg:{thread}:{ns}:{checkpoint}  the checkpoint with its metadata, its parent id and, while the
                                     record stays within INLINE_CHECKPOINT_MAX_BYTES, every channel
                                     value. Values that are None, a string, a number or a boolean
                                     are always serialized with the checkpoint itself.
      lgb:{thread}:{ns}:{channel}:{version}
                                     one channel value at one version, used only when the record
                                     would be too big for inline values: then every non-scalar
                                     value of that checkpoint is written here, alongside it, so the
                                     size limit applies to each value rather than to the whole
                                     state.
      lgh:{thread}:{ns}              the id of the newest checkpoint, carrying a copy of its record
                                     when that is small enough. The key-value API has no "latest"
                                     query, and the copy saves a second read on every turn.
      lgw:{thread}:{ns}:{checkpoint}:{task}:{idx}
                                     the writes one task made against a checkpoint. All regular
                                     writes of one put_writes call share one record, keyed by the
                                     first one's index, so a crash cannot leave a task with only
                                     some of its writes; each write to a special channel (error,
                                     interrupt, resume, scheduled) has its own record at its
                                     negative index. Every record is written unconditionally, one
                                     call each: langgraph saves a task's regular writes again only
                                     when it re-runs a task whose first save did not land, so the
                                     later one wins.

    Calls. The platform rate-limits each agent, so the call count is part of the design: a put is
    two writes (the record, then the head) while values fit inline; reading the newest checkpoint
    is the head plus the writes listing; put_writes is one write per record above. Inline values
    are rewritten by every put, so a long thread costs bytes rather than calls until its record
    outgrows the inline bound.

    Blobs are written before the checkpoint and the checkpoint before the head that points at it,
    so a crash can only leave the head one checkpoint behind, which resumes cleanly: that step's
    tasks have their writes saved and are not run again. State expires with the platform's
    retention, counted from each record's last write; reading a checkpoint whose blob has expired
    raises NasikoStateMalformedRow rather than returning it without that channel. A value above
    the platform's size limit raises NasikoStateTooLarge.
    """

    def __init__(self, kv: NasikoKV, *, serde: Any = None) -> None:
        if not _LANGGRAPH_AVAILABLE:
            raise ImportError("NasikoCheckpointSaver needs the langgraph-checkpoint package")
        super().__init__(serde=serde)
        self._kv = kv

    # --- reading ---------------------------------------------------------------------------

    def get_tuple(self, config: Any) -> Any:
        record = self._find_record(config)
        if record is None:
            return None
        writes = [write.value for write in self._kv.list(_writes_prefix(record))]
        blobs = self._load_blobs(record, {})
        explicit = config if config["configurable"].get("checkpoint_id") else None
        return self._tuple(record, writes, blobs, config=explicit)

    async def aget_tuple(self, config: Any) -> Any:
        record = await self._afind_record(config)
        if record is None:
            return None
        writes = [write.value async for write in self._kv.alist(_writes_prefix(record))]
        blobs = await self._aload_blobs(record, {})
        explicit = config if config["configurable"].get("checkpoint_id") else None
        return self._tuple(record, writes, blobs, config=explicit)

    def _find_record(self, config: Any) -> dict[str, Any] | None:
        """The checkpoint record ``config`` names, or the newest one, which the head carries."""
        thread_id, namespace = _thread_and_namespace(config)
        checkpoint_id = config["configurable"].get("checkpoint_id")
        if checkpoint_id is None:
            head = self._kv.get(_key(HEAD_FAMILY, thread_id, namespace))
            if head is None:
                return None
            checkpoint_id, embedded = _head_contents(head)
            if embedded is not None:
                return embedded
        stored = self._kv.get(_key(CHECKPOINT_FAMILY, thread_id, namespace, checkpoint_id))
        return None if stored is None else stored.value

    async def _afind_record(self, config: Any) -> dict[str, Any] | None:
        thread_id, namespace = _thread_and_namespace(config)
        checkpoint_id = config["configurable"].get("checkpoint_id")
        if checkpoint_id is None:
            head = await self._kv.aget(_key(HEAD_FAMILY, thread_id, namespace))
            if head is None:
                return None
            checkpoint_id, embedded = _head_contents(head)
            if embedded is not None:
                return embedded
        stored = await self._kv.aget(_key(CHECKPOINT_FAMILY, thread_id, namespace, checkpoint_id))
        return None if stored is None else stored.value

    def list(
        self,
        config: Any,
        *,
        filter: dict[str, Any] | None = None,
        before: Any = None,
        limit: int | None = None,
    ) -> Iterator[Any]:
        scope = _scope(config)
        checkpoints = [record.value for record in self._kv.list(_prefix(CHECKPOINT_FAMILY, *scope))]
        selected = self._select(checkpoints, config, filter, before, limit)
        writes = self._writes_by_checkpoint(list(self._kv.list(_prefix(WRITES_FAMILY, *scope))))
        loaded_blobs: dict[str, KVRecord | None] = {}  # checkpoints share unchanged values
        for record in selected:
            blobs = self._load_blobs(record, loaded_blobs)
            yield self._tuple(record, writes.get(_checkpoint_identity(record), []), blobs)

    async def alist(
        self,
        config: Any,
        *,
        filter: dict[str, Any] | None = None,
        before: Any = None,
        limit: int | None = None,
    ) -> AsyncIterator[Any]:
        scope = _scope(config)
        checkpoints = [record.value async for record in self._kv.alist(_prefix(CHECKPOINT_FAMILY, *scope))]
        selected = self._select(checkpoints, config, filter, before, limit)
        write_records = [record async for record in self._kv.alist(_prefix(WRITES_FAMILY, *scope))]
        writes = self._writes_by_checkpoint(write_records)
        loaded_blobs: dict[str, KVRecord | None] = {}
        for record in selected:
            blobs = await self._aload_blobs(record, loaded_blobs)
            yield self._tuple(record, writes.get(_checkpoint_identity(record), []), blobs)

    def _load_blobs(self, record: dict[str, Any], loaded: dict[str, KVRecord | None]) -> dict[str, Any]:
        """The blob records a checkpoint refers to, by channel. ``loaded`` caches across calls."""
        keys = _blob_keys(record)
        for key in keys.values():
            if key not in loaded:
                loaded[key] = self._kv.get(key)
        return {channel: loaded[key] for channel, key in keys.items()}

    async def _aload_blobs(
        self, record: dict[str, Any], loaded: dict[str, KVRecord | None]
    ) -> dict[str, Any]:
        keys = _blob_keys(record)
        for key in keys.values():
            if key not in loaded:
                loaded[key] = await self._kv.aget(key)
        return {channel: loaded[key] for channel, key in keys.items()}

    # --- writing ---------------------------------------------------------------------------

    # ``new_versions`` goes unused: a record carries every channel value or writes it alongside,
    # because whether an unchanged value was ever written on its own is not known here.

    def put(self, config: Any, checkpoint: Any, metadata: Any, new_versions: Any) -> Any:
        plan = self._planned_checkpoint(config, checkpoint, metadata, spill=False)
        try:
            self._write_checkpoint(plan)
        except NasikoStateTooLarge:
            if plan.spilled:
                raise
            self._write_checkpoint(self._planned_checkpoint(config, checkpoint, metadata, spill=True))
        return _checkpoint_config(*_checkpoint_identity(plan.record))

    async def aput(self, config: Any, checkpoint: Any, metadata: Any, new_versions: Any) -> Any:
        plan = self._planned_checkpoint(config, checkpoint, metadata, spill=False)
        try:
            await self._awrite_checkpoint(plan)
        except NasikoStateTooLarge:
            if plan.spilled:
                raise
            await self._awrite_checkpoint(self._planned_checkpoint(config, checkpoint, metadata, spill=True))
        return _checkpoint_config(*_checkpoint_identity(plan.record))

    def _write_checkpoint(self, plan: _CheckpointPlan) -> None:
        """Blobs, then the record, then the head: a crash leaves the head at most one behind."""
        for key, value in plan.blobs:
            self._kv.put(key, value)
        self._kv.put(plan.record_key, plan.record)
        try:
            self._kv.put(plan.head_key, plan.head)
        except NasikoStateTooLarge:
            self._kv.put(plan.head_key, _bare_head(plan.record))

    async def _awrite_checkpoint(self, plan: _CheckpointPlan) -> None:
        for key, value in plan.blobs:
            await self._kv.aput(key, value)
        await self._kv.aput(plan.record_key, plan.record)
        try:
            await self._kv.aput(plan.head_key, plan.head)
        except NasikoStateTooLarge:
            await self._kv.aput(plan.head_key, _bare_head(plan.record))

    def put_writes(
        self, config: Any, writes: Sequence[tuple[str, Any]], task_id: str, task_path: str = ""
    ) -> None:
        for key, record in self._planned_writes(config, writes, task_id, task_path):
            self._kv.put(key, record)

    async def aput_writes(
        self, config: Any, writes: Sequence[tuple[str, Any]], task_id: str, task_path: str = ""
    ) -> None:
        for key, record in self._planned_writes(config, writes, task_id, task_path):
            await self._kv.aput(key, record)

    def delete_thread(self, thread_id: str) -> None:
        for family in _THREAD_FAMILIES:
            for record in list(self._kv.list(_prefix(family, thread_id))):
                self._kv.delete(record.key)

    async def adelete_thread(self, thread_id: str) -> None:
        for family in _THREAD_FAMILIES:
            records = [record async for record in self._kv.alist(_prefix(family, thread_id))]
            for record in records:
                await self._kv.adelete(record.key)

    # --- records ---------------------------------------------------------------------------

    def _encode(self, value: Any) -> dict[str, str]:
        kind, data = self.serde.dumps_typed(value)
        if kind == "pickle":
            raise NasikoStateError("refusing to store a pickled value in the platform state")
        return {"type": kind, "data": base64.b64encode(data).decode("ascii")}

    def _decode(self, encoded: Mapping[str, str]) -> Any:
        if encoded["type"] == "pickle":
            raise NasikoStateError("refusing to load a pickled value from the platform state")
        return self.serde.loads_typed((encoded["type"], base64.b64decode(encoded["data"])))

    def _planned_checkpoint(
        self, config: Any, checkpoint: Any, metadata: Any, *, spill: bool
    ) -> _CheckpointPlan:
        """What one put writes. Values go inline unless ``spill`` or the record would be too big."""
        thread_id, namespace = _thread_and_namespace(config)
        scalars, values = _split_channel_values(checkpoint["channel_values"])
        encoded = {channel: self._encode(value) for channel, value in values.items()}
        record = {
            "thread_id": thread_id,
            "checkpoint_ns": namespace,
            "checkpoint_id": checkpoint["id"],
            "parent_checkpoint_id": config["configurable"].get("checkpoint_id"),
            "checkpoint": self._encode({**checkpoint, "channel_values": scalars}),
            "metadata": self._encode(get_checkpoint_metadata(config, metadata)),
            "values": encoded,
            "blobs": {},
        }
        spill = spill or _json_size(record) > INLINE_CHECKPOINT_MAX_BYTES
        blobs: list[tuple[str, Any]] = []
        if spill:
            versions = checkpoint["channel_versions"]
            record = {**record, "values": {}, "blobs": {channel: versions[channel] for channel in values}}
            blobs = [
                (_key(BLOB_FAMILY, thread_id, namespace, channel, str(versions[channel])), value)
                for channel, value in encoded.items()
            ]
        return _CheckpointPlan(
            blobs=blobs,
            record_key=_key(CHECKPOINT_FAMILY, thread_id, namespace, checkpoint["id"]),
            record=record,
            head_key=_key(HEAD_FAMILY, thread_id, namespace),
            head=_head(record),
            spilled=spill,
        )

    @_reads_stored_records
    def _tuple(
        self,
        record: dict[str, Any],
        write_records: list[dict[str, Any]],
        blobs: Mapping[str, KVRecord | None],
        *,
        config: Any = None,
    ) -> Any:
        thread_id, namespace = record["thread_id"], record["checkpoint_ns"]
        parent_id = record["parent_checkpoint_id"]
        checkpoint = self._decode(record["checkpoint"])
        inline = {channel: self._decode(value) for channel, value in record.get("values", {}).items()}
        checkpoint["channel_values"] = {
            **checkpoint["channel_values"],
            **inline,
            **self._blob_values(record, blobs),
        }
        return CheckpointTuple(
            config=config or _checkpoint_config(thread_id, namespace, record["checkpoint_id"]),
            checkpoint=checkpoint,
            metadata=self._decode(record["metadata"]),
            parent_config=_checkpoint_config(thread_id, namespace, parent_id) if parent_id else None,
            pending_writes=self._pending_writes(write_records),
        )

    def _blob_values(self, record: dict[str, Any], blobs: Mapping[str, KVRecord | None]) -> dict[str, Any]:
        values = {}
        for channel, blob in blobs.items():
            if blob is None:
                raise NasikoStateMalformedRow(
                    f"checkpoint {record['checkpoint_id']!r} of thread {record['thread_id']!r} refers to "
                    f"channel {channel!r} at version {record['blobs'][channel]!r}, which is gone (expired "
                    "or deleted)"
                )
            values[channel] = self._decode(blob.value)
        return values

    def _pending_writes(self, write_records: list[dict[str, Any]]) -> list[tuple[str, str, Any]]:
        """Flatten write records into langgraph's (task, channel, value), ordered by task then index."""
        entries = []
        for record in write_records:
            for write in record["writes"]:
                entries.append((record["task_id"], write["idx"], write["channel"], write["value"]))
        entries.sort(key=lambda entry: (entry[0], entry[1]))
        return [(task_id, channel, self._decode(value)) for task_id, _, channel, value in entries]

    def _planned_writes(
        self, config: Any, writes: Sequence[tuple[str, Any]], task_id: str, task_path: str
    ) -> list[tuple[str, dict[str, Any]]]:
        thread_id, namespace = _thread_and_namespace(config)
        checkpoint_id = config["configurable"]["checkpoint_id"]
        regular: list[tuple[int, str, Any]] = []
        special: dict[int, tuple[int, str, Any]] = {}
        for position, (channel, value) in enumerate(writes):
            if channel in WRITES_IDX_MAP:
                special[WRITES_IDX_MAP[channel]] = (WRITES_IDX_MAP[channel], channel, value)
            else:
                regular.append((position, channel, value))

        def planned(entries: list[tuple[int, str, Any]]) -> tuple[str, dict[str, Any]]:
            record = {
                "thread_id": thread_id,
                "checkpoint_ns": namespace,
                "checkpoint_id": checkpoint_id,
                "task_id": task_id,
                "task_path": task_path,
                "writes": [
                    {"idx": idx, "channel": channel, "value": self._encode(value)}
                    for idx, channel, value in entries
                ],
            }
            key = _key(WRITES_FAMILY, thread_id, namespace, checkpoint_id, task_id, str(entries[0][0]))
            return key, record

        plan = [planned(regular)] if regular else []
        return plan + [planned([entry]) for entry in special.values()]

    @_reads_stored_records
    def _writes_by_checkpoint(
        self, write_records: list[KVRecord]
    ) -> dict[tuple[str, str, str], list[dict[str, Any]]]:
        grouped: dict[tuple[str, str, str], list[dict[str, Any]]] = {}
        for item in write_records:
            grouped.setdefault(_checkpoint_identity(item.value), []).append(item.value)
        return grouped

    @_reads_stored_records
    def _select(
        self,
        records: list[dict[str, Any]],
        config: Any,
        filter: dict[str, Any] | None,
        before: Any,
        limit: int | None,
    ) -> list[dict[str, Any]]:
        """The checkpoints list() yields: newest first, narrowed by id, `before`, metadata and `limit`."""
        wanted_id = config["configurable"].get("checkpoint_id") if config else None
        before_id = before["configurable"].get("checkpoint_id") if before else None
        selected = []
        for record in sorted(records, key=lambda r: r["checkpoint_id"], reverse=True):
            if wanted_id and record["checkpoint_id"] != wanted_id:
                continue
            if before_id and record["checkpoint_id"] >= before_id:
                continue
            if filter and not self._matches(record, filter):
                continue
            selected.append(record)
        return selected if limit is None else selected[: max(limit, 0)]

    def _matches(self, record: dict[str, Any], filter: dict[str, Any]) -> bool:
        metadata = self._decode(record["metadata"])
        return all(metadata.get(key) == value for key, value in filter.items())


def _checkpoint_config(thread_id: str, namespace: str, checkpoint_id: str) -> dict[str, Any]:
    return {
        "configurable": {"thread_id": thread_id, "checkpoint_ns": namespace, "checkpoint_id": checkpoint_id}
    }


def _checkpoint_identity(record: Mapping[str, Any]) -> tuple[str, str, str]:
    return record["thread_id"], record["checkpoint_ns"], record["checkpoint_id"]


# Heads first, so a half-deleted thread reads as empty rather than pointing at nothing.
_THREAD_FAMILIES = (HEAD_FAMILY, CHECKPOINT_FAMILY, WRITES_FAMILY, BLOB_FAMILY)


def _is_scalar_value(value: Any) -> bool:
    """Scalars are serialized with the checkpoint itself, as langgraph's Postgres saver keeps them."""
    return value is None or isinstance(value, (str, int, float, bool))


def _split_channel_values(values: Mapping[str, Any]) -> tuple[dict[str, Any], dict[str, Any]]:
    scalars = {channel: value for channel, value in values.items() if _is_scalar_value(value)}
    others = {channel: value for channel, value in values.items() if not _is_scalar_value(value)}
    return scalars, others


def _json_size(value: Any) -> int:
    """Roughly the bytes the state API measures a value at: its compact JSON encoding."""
    return len(json.dumps(value, separators=(",", ":")))


@dataclass(frozen=True)
class _CheckpointPlan:
    """The records one put writes, in the order it writes them."""

    blobs: list[tuple[str, Any]]
    record_key: str
    record: dict[str, Any]
    head_key: str
    head: dict[str, Any]
    spilled: bool


def _head(record: dict[str, Any]) -> dict[str, Any]:
    """The head names the newest checkpoint and carries its record when that is small enough."""
    if _json_size(record) > INLINE_CHECKPOINT_MAX_BYTES:
        return _bare_head(record)
    return {"checkpoint_id": record["checkpoint_id"], "checkpoint": record}


def _bare_head(record: Mapping[str, Any]) -> dict[str, Any]:
    return {"checkpoint_id": record["checkpoint_id"]}


@_reads_stored_records
def _head_contents(head: KVRecord) -> tuple[str, dict[str, Any] | None]:
    """The newest checkpoint's id, and its record when the head carries one."""
    checkpoint_id = head.value["checkpoint_id"]
    embedded = head.value.get("checkpoint")
    if embedded is not None and embedded["checkpoint_id"] != checkpoint_id:
        raise NasikoStateMalformedRow(
            f"the head of thread {embedded['thread_id']!r} names checkpoint {checkpoint_id!r} but "
            f"carries {embedded['checkpoint_id']!r}"
        )
    return checkpoint_id, embedded


@_reads_stored_records
def _writes_prefix(record: Mapping[str, Any]) -> str:
    return _prefix(WRITES_FAMILY, *_checkpoint_identity(record))


@_reads_stored_records
def _blob_keys(record: Mapping[str, Any]) -> dict[str, str]:
    thread_id, namespace = record["thread_id"], record["checkpoint_ns"]
    return {
        channel: _key(BLOB_FAMILY, thread_id, namespace, channel, str(version))
        for channel, version in record["blobs"].items()
    }


def make_checkpointer() -> Any:
    """The checkpointer a langgraph agent compiles its graph with.

    The platform's durable one when NASIKO_STATE_URL is set, otherwise langgraph's own
    ``MemorySaver``, built exactly as an agent would build it by hand. Like make_task_store, the
    durable one registers with the platform in the background right away.
    """
    client = NasikoStateClient.from_env()
    if client is None:
        from langgraph.checkpoint.memory import MemorySaver

        return MemorySaver()
    logger.info("langgraph checkpoints are kept in the platform state API: %r", client)
    saver = NasikoCheckpointSaver(NasikoKV(client))
    client.register_in_background("kv")
    return saver
