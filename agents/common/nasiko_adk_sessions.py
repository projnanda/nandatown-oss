"""Durable Google ADK sessions over the platform's state API.

ADK keeps conversations in a session service. The stock ``InMemorySessionService`` loses every
session when the agent restarts and cannot be shared by replicas. ``NasikoSessionService`` keeps
them in the platform's key-value state instead:

    from nasiko_adk_sessions import session_service_from_env

    runner = Runner(
        agent=agent,
        app_name=APP_NAME,
        session_service=session_service_from_env(),   # was: InMemorySessionService()
    )

Configuration is the same environment as nasiko_state.py (``NASIKO_STATE_URL`` and
``NASIKO_AGENT_TOKEN``). With ``NASIKO_STATE_URL`` unset the factory returns ADK's own
``InMemorySessionService`` and nothing is registered, so the same code runs on a laptop.

Layout. Every component is percent-encoded, so '/' only ever separates components. The whole
encoded key may be at most 512 bytes and percent-encoding can triple an id's length (every byte
of a non-ASCII or reserved character becomes three), so keep app, user and session ids short:

    adk/session/{app}/{user}/{session_id}   one session: its own JSON serialization
    adk/user/{app}/{user}                   state shared by all of a user's sessions ("user:" keys)
    adk/app/{app}                           state shared by every session of the app ("app:" keys)

ADK sessions are not task durability, so this registers the key-value capability only; an agent is
never counted as having durable tasks because it uses this module.

Concurrency. Several replicas can append to one session. Each append reads the stored row, applies
the event to it and writes it back conditional on the version it read (If-Match). When another
replica wrote first, the append is re-read and re-applied, up to MAX_CONFLICT_ATTEMPTS times, and
then fails with NasikoStateError instead of dropping the event.

An append writes the shared app:/user: state before the session row, so a failure between the
two leaves that shared delta one event ahead of the session. Retrying the append is safe: the
delta is merged again with the same values and the event is then stored once.

This file is vendored next to the agent's main module, like nasiko_state.py (which it imports);
oss/agents/common is canonical and check_vendored.py fails when a copy drifts.
"""

from __future__ import annotations

import logging
import time
import uuid
from collections.abc import Callable
from typing import Any, Optional

from google.adk.errors.already_exists_error import AlreadyExistsError
from google.adk.errors.session_not_found_error import SessionNotFoundError
from google.adk.events.event import Event
from google.adk.sessions import InMemorySessionService
from google.adk.sessions.base_session_service import (
    BaseSessionService,
    GetSessionConfig,
    ListSessionsResponse,
)
from google.adk.sessions.session import Session
from google.adk.sessions.state import State
from nasiko_state import (
    MAX_PAGE_SIZE,
    NasikoStateClient,
    NasikoStateConflict,
    NasikoStateError,
    NasikoStateMalformedRow,
    _percent_encode,
)

logger = logging.getLogger("nasiko_adk_sessions")

# How often one append retries after losing the version race before it gives up.
MAX_CONFLICT_ATTEMPTS = 5

# The version the state API reports for a row that does not exist (or has expired).
ABSENT_VERSION = 0

SESSION_FAMILY = "adk/session"
USER_STATE_FAMILY = "adk/user"
APP_STATE_FAMILY = "adk/app"

# Returned by an update function that found nothing to change, so no write is made.
_UNCHANGED: Any = object()

StateDelta = dict[str, Any]


def _key(family: str, *parts: str) -> str:
    return "/".join([family, *(_percent_encode(part) for part in parts)])


def _split_state(state: dict[str, Any]) -> tuple[StateDelta, StateDelta, StateDelta]:
    """Split a state dict into (app, user, session) parts with the scope prefixes removed.

    ``temp:`` keys live for one invocation only and are never stored.
    """
    app: StateDelta = {}
    user: StateDelta = {}
    session: StateDelta = {}
    for key, value in state.items():
        if key.startswith(State.APP_PREFIX):
            app[key.removeprefix(State.APP_PREFIX)] = value
        elif key.startswith(State.USER_PREFIX):
            user[key.removeprefix(State.USER_PREFIX)] = value
        elif not key.startswith(State.TEMP_PREFIX):
            session[key] = value
    return app, user, session


def _serialize(session: Session) -> dict[str, Any]:
    return session.model_dump(mode="json", by_alias=True, exclude_none=True)


def _load(key: str, value: Any) -> Session:
    try:
        return Session.model_validate(value)
    except ValueError as exc:
        raise NasikoStateMalformedRow(f"stored session {key!r} is not a valid ADK session: {exc}") from exc


def _with_shared_state(session: Session, app_state: StateDelta, user_state: StateDelta) -> Session:
    """The session as ADK hands it out: its own state plus the app and user state, prefixed."""
    session.state.update({State.APP_PREFIX + key: value for key, value in app_state.items()})
    session.state.update({State.USER_PREFIX + key: value for key, value in user_state.items()})
    return session


def _trim_events(session: Session, config: GetSessionConfig | None) -> None:
    """Apply get_session's event filters the way InMemorySessionService does."""
    if config is None:
        return
    if config.num_recent_events is not None:
        session.events = session.events[-config.num_recent_events :] if config.num_recent_events else []
    if config.after_timestamp:
        # Keep the events after the last one older than the cutoff, as the in-memory service
        # does, so a late event is never hidden behind an earlier one.
        last_old = max((i for i, e in enumerate(session.events) if e.timestamp < config.after_timestamp), default=-1)
        session.events = session.events[last_old + 1 :]


def _already_stored(session: Session, event: Event) -> bool:
    """Whether an equal event is in the session: a re-delivery, not a new event."""
    wanted = event.model_dump(mode="json")
    return any(stored.id == event.id and stored.model_dump(mode="json") == wanted for stored in session.events)


def _session_with_event(key: str, value: Any, event: Event, session_delta: StateDelta) -> Any:
    if value is None:
        raise SessionNotFoundError(f"Session {key} was deleted while an event was being appended.")
    stored = _load(key, value)
    if _already_stored(stored, event):
        return _UNCHANGED  # another attempt of this append already landed
    stored.events.append(event)
    stored.state.update(session_delta)
    stored.last_update_time = event.timestamp
    return _serialize(stored)


class NasikoSessionService(BaseSessionService):
    """An ADK session service that keeps sessions in the platform's key-value state."""

    def __init__(self, client: NasikoStateClient) -> None:
        self._client = client

    async def aclose(self) -> None:
        await self._client.aclose()

    # --- sessions --------------------------------------------------------------------------

    async def create_session(
        self,
        *,
        app_name: str,
        user_id: str,
        state: Optional[dict[str, Any]] = None,
        session_id: Optional[str] = None,
    ) -> Session:
        session_id = session_id.strip() if session_id else None
        app_delta, user_delta, session_state = _split_state(state or {})
        await self._merge_shared(APP_STATE_FAMILY, (app_name,), app_delta)
        await self._merge_shared(USER_STATE_FAMILY, (app_name, user_id), user_delta)
        session = Session(
            id=session_id or str(uuid.uuid4()),
            app_name=app_name,
            user_id=user_id,
            state=session_state,
            last_update_time=time.time(),
        )
        key = _key(SESSION_FAMILY, app_name, user_id, session.id)
        try:
            # If-Match 0 means "only if the row is not there yet", so a racing create cannot overwrite.
            await self._client.aput_kv(key, _serialize(session), if_match=ABSENT_VERSION)
        except NasikoStateConflict as exc:
            raise AlreadyExistsError(f"Session with id {session.id} already exists.") from exc
        return await self._with_shared(session)

    async def get_session(
        self,
        *,
        app_name: str,
        user_id: str,
        session_id: str,
        config: Optional[GetSessionConfig] = None,
    ) -> Optional[Session]:
        session_id = session_id.strip() if session_id else session_id
        key = _key(SESSION_FAMILY, app_name, user_id, session_id)
        record = await self._client.aget_kv(key)
        if record is None:
            return None
        session = _load(key, record.value)
        _trim_events(session, config)
        return await self._with_shared(session)

    async def list_sessions(self, *, app_name: str, user_id: Optional[str] = None) -> ListSessionsResponse:
        prefix = _key(SESSION_FAMILY, app_name, *([user_id] if user_id is not None else [])) + "/"
        app_state = await self._read_shared(APP_STATE_FAMILY, (app_name,))
        user_states: dict[str, StateDelta] = {}
        sessions = []
        cursor: str | None = None
        while True:
            page = await self._client.alist_kv(prefix=prefix, limit=MAX_PAGE_SIZE, cursor=cursor)
            for record in page.items:
                session = _load(record.key, record.value)
                session.events = []  # a listing never carries events
                if session.user_id not in user_states:
                    user_states[session.user_id] = await self._read_shared(
                        USER_STATE_FAMILY, (app_name, session.user_id)
                    )
                sessions.append(_with_shared_state(session, app_state, user_states[session.user_id]))
            if page.next_cursor is None:
                break
            cursor = page.next_cursor
        sessions.sort(key=lambda s: (s.last_update_time, s.user_id, s.id))
        return ListSessionsResponse(sessions=sessions)

    async def delete_session(self, *, app_name: str, user_id: str, session_id: str) -> None:
        session_id = session_id.strip() if session_id else session_id
        await self._client.adelete_kv(_key(SESSION_FAMILY, app_name, user_id, session_id))

    async def get_user_state(self, *, app_name: str, user_id: str) -> dict[str, Any]:
        return await self._read_shared(USER_STATE_FAMILY, (app_name, user_id))

    # --- events ----------------------------------------------------------------------------

    async def append_event(self, session: Session, event: Event) -> Event:
        if event.partial:
            return event
        key = _key(SESSION_FAMILY, session.app_name, session.user_id, session.id)
        stored = await self._client.aget_kv(key)
        if stored is None:
            raise SessionNotFoundError(f"Session {session.id} not found.")
        if _already_stored(_load(key, stored.value), event):
            return event  # a re-delivery must not apply its state delta twice

        # The base class applies the event to the caller's own copy and trims temp: state from it.
        await super().append_event(session=session, event=event)
        session.last_update_time = event.timestamp
        app_delta, user_delta, session_delta = _split_state(
            event.actions.state_delta if event.actions and event.actions.state_delta else {}
        )
        # Shared state first: the event's presence in the session is what stops a retry from
        # applying it again, so it has to be written last.
        await self._merge_shared(APP_STATE_FAMILY, (session.app_name,), app_delta)
        await self._merge_shared(USER_STATE_FAMILY, (session.app_name, session.user_id), user_delta)
        await self._update_row(key, lambda value: _session_with_event(key, value, event, session_delta))
        return event

    # --- shared (app and user) state -------------------------------------------------------

    async def _read_shared(self, family: str, parts: tuple[str, ...]) -> StateDelta:
        record = await self._client.aget_kv(_key(family, *parts))
        return {} if record is None else dict(record.value)

    async def _merge_shared(self, family: str, parts: tuple[str, ...], delta: StateDelta) -> None:
        if not delta:
            return
        await self._update_row(_key(family, *parts), lambda value: {**(value or {}), **delta})

    async def _with_shared(self, session: Session) -> Session:
        app_state = await self._read_shared(APP_STATE_FAMILY, (session.app_name,))
        user_state = await self._read_shared(USER_STATE_FAMILY, (session.app_name, session.user_id))
        return _with_shared_state(session, app_state, user_state)

    # --- optimistic concurrency ------------------------------------------------------------

    async def _update_row(self, key: str, change: Callable[[Any], Any]) -> None:
        """Read a row, apply ``change`` to its value, write it back if no one else wrote meanwhile.

        ``change`` gets None for a missing row and returns the new value, or _UNCHANGED.
        """
        for _ in range(MAX_CONFLICT_ATTEMPTS):
            record = await self._client.aget_kv(key)
            value = change(None if record is None else record.value)
            if value is _UNCHANGED:
                return
            try:
                # A missing row is written with If-Match 0 (only if still absent), never unconditionally,
                # so two first writers cannot overwrite each other.
                await self._client.aput_kv(
                    key, value, if_match=ABSENT_VERSION if record is None else record.version
                )
            except NasikoStateConflict:
                logger.info("row %s changed by another writer; re-reading", key)
                continue
            return
        raise NasikoStateError(
            f"row {key!r} kept changing under {MAX_CONFLICT_ATTEMPTS} attempts to update it; "
            "the update was not applied"
        )


def session_service_from_env() -> BaseSessionService:
    """The session service an ADK agent hands to its Runner.

    The platform's durable service when NASIKO_STATE_URL is set, otherwise ADK's in-memory one.
    Nothing contacts the platform until the first session is stored.
    """
    client = NasikoStateClient.from_env()
    if client is None:
        return InMemorySessionService()
    logger.info("ADK sessions are kept in the platform state API: %r", client)
    return NasikoSessionService(client)
