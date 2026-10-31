"""NasikoSessionService: ADK sessions kept in the platform's key-value state."""

from __future__ import annotations

import asyncio
import importlib.util
import json
from pathlib import Path

import pytest

pytest.importorskip("google.adk")

from fake_state_server import BASE_URL, TOKEN, Row  # noqa: E402
from google.adk.errors.already_exists_error import AlreadyExistsError  # noqa: E402
from google.adk.errors.session_not_found_error import SessionNotFoundError  # noqa: E402
from google.adk.events.event import Event  # noqa: E402
from google.adk.events.event_actions import EventActions  # noqa: E402
from google.adk.sessions import InMemorySessionService  # noqa: E402
from google.adk.sessions.base_session_service import GetSessionConfig  # noqa: E402
from google.genai import types as genai_types  # noqa: E402
from nasiko_adk_sessions import (  # noqa: E402
    MAX_CONFLICT_ATTEMPTS,
    NasikoSessionService,
    session_service_from_env,
)
from nasiko_state import NasikoStateError, NasikoStateMalformedRow  # noqa: E402

pytestmark = pytest.mark.anyio

APP = "research"


@pytest.fixture
def service(client) -> NasikoSessionService:
    return NasikoSessionService(client)


def text_event(text: str, *, author: str = "user", timestamp: float = 100.0, delta=None, **fields) -> Event:
    return Event(
        author=author,
        timestamp=timestamp,
        content=genai_types.Content(role="user", parts=[genai_types.Part(text=text)]),
        actions=EventActions(state_delta=delta or {}),
        **fields,
    )


async def new_session(service, user="u1", session_id="s1", state=None):
    return await service.create_session(app_name=APP, user_id=user, session_id=session_id, state=state)


async def test_a_created_session_reads_back(service):
    created = await new_session(service, state={"topic": "tides"})

    loaded = await service.get_session(app_name=APP, user_id="u1", session_id="s1")

    assert (loaded.id, loaded.app_name, loaded.user_id) == ("s1", APP, "u1")
    assert loaded.state == {"topic": "tides"}
    assert loaded.last_update_time == created.last_update_time


async def test_a_missing_session_is_none(service):
    assert await service.get_session(app_name=APP, user_id="u1", session_id="nope") is None


async def test_a_generated_id_is_used_when_none_is_given(service):
    created = await service.create_session(app_name=APP, user_id="u1")

    assert created.id
    assert await service.get_session(app_name=APP, user_id="u1", session_id=created.id) is not None


async def test_creating_an_existing_session_is_refused(service):
    await new_session(service)

    with pytest.raises(AlreadyExistsError):
        await new_session(service)


async def test_a_create_racing_an_existing_row_is_refused_and_leaves_it_alone(service, state_server):
    key = f"adk/session/{APP}/u1/s1"

    async def another_replica_creates_first(request):
        # Between this create's merge of shared state and its session write, a second replica wins.
        if request.method == "PUT" and request.url.path.endswith("s1") and key not in state_server.kv:
            state_server.kv[key] = Row({"key": key, "value": {"id": "s1", "theirs": True}}, 1, 99)

    state_server.gate = another_replica_creates_first

    with pytest.raises(AlreadyExistsError):
        await new_session(service)

    assert state_server.kv[key].value["value"] == {"id": "s1", "theirs": True}


async def test_two_concurrent_first_merges_of_different_user_keys_both_survive(service, state_server):
    first = await new_session(service, session_id="a")
    second = await new_session(service, session_id="b")
    user_key = f"adk/user/{APP}/u1"
    state_server.kv.pop(user_key, None)  # the shared row does not exist yet for either writer
    reads_done = asyncio.Event()
    waiting = {"count": 0}

    async def hold_both_first_writes_until_both_have_read(request):
        # Each merge has read the missing row by the time its first write arrives; neither write is
        # served until both have arrived: the lost-update interleaving.
        if request.method == "PUT" and "adk%2Fuser" in request.url.raw_path.decode():
            waiting["count"] += 1
            if waiting["count"] == 2:
                reads_done.set()
            await reads_done.wait()

    state_server.gate = hold_both_first_writes_until_both_have_read

    await asyncio.gather(
        service.append_event(first, text_event("one", timestamp=1.0, delta={"user:a": 1})),
        service.append_event(second, text_event("two", timestamp=2.0, delta={"user:b": 2})),
    )

    assert state_server.kv[user_key].value["value"] == {"a": 1, "b": 2}


async def test_the_row_is_stored_under_the_documented_key_as_the_sessions_own_json(service, state_server):
    await new_session(service, user="user/1", session_id="s:1")

    [key] = [k for k in state_server.kv if k.startswith("adk/session/")]

    assert key == f"adk/session/{APP}/user%2F1/s%3A1"
    assert state_server.kv[key].value["value"]["id"] == "s:1"


async def test_events_survive_a_new_service_instance(service, client):
    await new_session(service)
    session = await service.get_session(app_name=APP, user_id="u1", session_id="s1")
    await service.append_event(session, text_event("hello", delta={"seen": 1}))

    fresh = NasikoSessionService(client)  # a restarted replica
    loaded = await fresh.get_session(app_name=APP, user_id="u1", session_id="s1")

    assert [e.content.parts[0].text for e in loaded.events] == ["hello"]
    assert loaded.state == {"seen": 1}
    assert loaded.last_update_time == 100.0


async def test_rich_events_round_trip_exactly(service):
    session = await new_session(service)
    call = genai_types.Part(function_call=genai_types.FunctionCall(name="search", args={"q": "x"}))
    blob = genai_types.Part(inline_data=genai_types.Blob(mime_type="image/png", data=b"\x00\xff\x10"))
    event = Event(
        author="agent",
        timestamp=5.0,
        content=genai_types.Content(role="model", parts=[call, blob]),
        actions=EventActions(state_delta={"k": [1, {"a": None}]}),
    )
    await service.append_event(session, event)

    loaded = await service.get_session(app_name=APP, user_id="u1", session_id="s1")

    assert loaded.events == [event]


async def test_the_callers_session_is_updated_like_the_in_memory_service_does(service):
    session = await new_session(service)

    await service.append_event(session, text_event("hi", delta={"a": 1}))

    assert len(session.events) == 1
    assert session.state == {"a": 1}
    assert session.last_update_time == 100.0


async def test_partial_events_are_not_stored(service, state_server):
    session = await new_session(service)
    writes_before = len(state_server.writes("kv"))

    await service.append_event(session, text_event("typing", partial=True))

    assert len(state_server.writes("kv")) == writes_before
    assert session.events == []


async def test_temp_state_is_never_stored(service):
    session = await new_session(service)

    await service.append_event(session, text_event("x", delta={"temp:scratch": 1, "keep": 2}))
    loaded = await service.get_session(app_name=APP, user_id="u1", session_id="s1")

    assert loaded.state == {"keep": 2}
    assert session.state["temp:scratch"] == 1  # still visible to the current invocation


async def test_appending_to_an_unknown_session_fails(service):
    ghost = await new_session(service, session_id="ghost")
    await service.delete_session(app_name=APP, user_id="u1", session_id="ghost")

    with pytest.raises(SessionNotFoundError):
        await service.append_event(ghost, text_event("hi"))


async def test_a_redelivered_event_is_applied_once(service):
    session = await new_session(service)
    event = text_event("once", delta={"count": 1})

    await service.append_event(session, event)
    await service.append_event(session, event)
    loaded = await service.get_session(app_name=APP, user_id="u1", session_id="s1")

    assert len(loaded.events) == 1


async def test_user_and_app_state_are_shared_across_sessions(service):
    first = await new_session(service, session_id="s1")
    await service.append_event(first, text_event("a", delta={"user:name": "Ada", "app:theme": "dark", "own": 1}))

    second = await new_session(service, session_id="s2")
    other_user = await new_session(service, user="u2", session_id="s3")

    assert second.state == {"user:name": "Ada", "app:theme": "dark"}
    assert other_user.state == {"app:theme": "dark"}
    assert await service.get_user_state(app_name=APP, user_id="u1") == {"name": "Ada"}
    stored = await service.get_session(app_name=APP, user_id="u1", session_id="s1")
    assert stored.state == {"own": 1, "user:name": "Ada", "app:theme": "dark"}


async def test_initial_scoped_state_is_split_on_creation(service, state_server):
    await new_session(service, state={"user:lang": "en", "app:v": 2, "mine": 3})

    assert state_server.kv[f"adk/user/{APP}/u1"].value["value"] == {"lang": "en"}
    assert state_server.kv[f"adk/app/{APP}"].value["value"] == {"v": 2}
    assert state_server.kv[f"adk/session/{APP}/u1/s1"].value["value"]["state"] == {"mine": 3}


async def test_get_session_config_filters_events(service):
    session = await new_session(service)
    for n in range(4):
        await service.append_event(session, text_event(f"e{n}", timestamp=10.0 + n))

    async def texts(config):
        loaded = await service.get_session(app_name=APP, user_id="u1", session_id="s1", config=config)
        return [e.content.parts[0].text for e in loaded.events]

    assert await texts(GetSessionConfig(num_recent_events=2)) == ["e2", "e3"]
    assert await texts(GetSessionConfig(num_recent_events=0)) == []
    assert await texts(GetSessionConfig(after_timestamp=12.0)) == ["e2", "e3"]
    assert await texts(None) == ["e0", "e1", "e2", "e3"]


async def test_listing_is_scoped_ordered_and_carries_no_events(service):
    old = await new_session(service, user="u1", session_id="old")
    await service.append_event(old, text_event("a", timestamp=10.0))
    fresh = await new_session(service, user="u2", session_id="fresh")
    await service.append_event(fresh, text_event("b", timestamp=20.0, delta={"app:x": 1}))
    await service.create_session(app_name="other-app", user_id="u1", session_id="foreign")

    everyone = await service.list_sessions(app_name=APP)
    one_user = await service.list_sessions(app_name=APP, user_id="u1")

    assert [s.id for s in everyone.sessions] == ["old", "fresh"]
    assert all(s.events == [] for s in everyone.sessions)
    assert [s.id for s in one_user.sessions] == ["old"]
    assert one_user.sessions[0].state == {"app:x": 1}


async def test_listing_orders_oldest_update_first(service):
    for name, stamp in (("b", 30.0), ("a", 10.0), ("c", 20.0)):
        session = await new_session(service, session_id=name)
        await service.append_event(session, text_event(name, timestamp=stamp))

    listed = await service.list_sessions(app_name=APP, user_id="u1")

    assert [s.id for s in listed.sessions] == ["a", "c", "b"]


async def test_a_user_id_that_is_a_prefix_of_another_does_not_leak_into_its_listing(service):
    await new_session(service, user="ann", session_id="s1")
    await new_session(service, user="ann/bob", session_id="s2")

    listed = await service.list_sessions(app_name=APP, user_id="ann")

    assert [s.id for s in listed.sessions] == ["s1"]


async def test_delete_removes_the_session_and_is_idempotent(service):
    await new_session(service)

    await service.delete_session(app_name=APP, user_id="u1", session_id="s1")
    await service.delete_session(app_name=APP, user_id="u1", session_id="s1")

    assert await service.get_session(app_name=APP, user_id="u1", session_id="s1") is None


async def test_a_conflicting_append_rereads_and_reapplies_the_event(service, state_server):
    session = await new_session(service)
    requests_before = len(state_server.requests)
    key = f"adk/session/{APP}/u1/s1"
    racing = {"armed": True}

    async def another_replica_appends_first(request):
        # Just before this replica's first conditional write, a second replica's event lands.
        if request.method != "PUT" or "if-match" not in request.headers or not racing.pop("armed", False):
            return
        row = state_server.kv[key]
        row.value["value"]["events"].append(text_event("theirs", timestamp=1.0).model_dump(mode="json", exclude_none=True))
        row.version += 1

    state_server.gate = another_replica_appends_first

    await service.append_event(session, text_event("mine", timestamp=2.0))
    loaded = await service.get_session(app_name=APP, user_id="u1", session_id="s1")

    assert [e.content.parts[0].text for e in loaded.events] == ["theirs", "mine"]
    conditional = [
        r for r in state_server.requests[requests_before:] if r.method == "PUT" and r.path.endswith("s1")
    ]
    assert len(conditional) == 2


async def test_an_append_that_keeps_conflicting_fails_loudly(service, state_server):
    session = await new_session(service)
    requests_before = len(state_server.requests)
    state_server.fail(412, times=MAX_CONFLICT_ATTEMPTS + 5, when=lambda r: r.method == "PUT")

    with pytest.raises(NasikoStateError, match="kept changing"):
        await service.append_event(session, text_event("never lands"))

    puts = [r for r in state_server.requests[requests_before:] if r.method == "PUT" and "if-match" in r.headers]
    assert len(puts) == MAX_CONFLICT_ATTEMPTS


async def test_a_damaged_row_is_reported_not_half_decoded(service, state_server):
    await new_session(service)
    state_server.kv[f"adk/session/{APP}/u1/s1"].value["value"] = {"id": 7}

    with pytest.raises(NasikoStateMalformedRow):
        await service.get_session(app_name=APP, user_id="u1", session_id="s1")


async def test_only_the_kv_capability_is_registered(service, state_server):
    session = await new_session(service)
    await service.append_event(session, text_event("hi"))
    await service.get_session(app_name=APP, user_id="u1", session_id="s1")

    assert state_server.registrations
    assert all(r["capabilities"] == ["kv"] for r in state_server.registrations)
    assert not state_server.tasks


async def test_every_request_carries_the_agent_token(service, state_server):
    await new_session(service)

    assert {r.headers["authorization"] for r in state_server.requests} == {f"Bearer {TOKEN}"}


class TestFactory:
    def test_unset_url_gives_the_in_memory_service_and_no_traffic(self, monkeypatch):
        import httpx

        def refuse(self, *args, **kwargs):
            raise AssertionError("the state API was contacted")

        monkeypatch.setattr(httpx.Client, "request", refuse)
        monkeypatch.setattr(httpx.AsyncClient, "request", refuse)

        assert type(session_service_from_env()) is InMemorySessionService

    def test_a_token_alone_changes_nothing(self, monkeypatch):
        monkeypatch.setenv("NASIKO_AGENT_TOKEN", TOKEN)

        assert type(session_service_from_env()) is InMemorySessionService

    def test_with_the_url_and_token_it_is_the_durable_service(self, monkeypatch):
        monkeypatch.setenv("NASIKO_STATE_URL", BASE_URL)
        monkeypatch.setenv("NASIKO_AGENT_TOKEN", TOKEN)

        assert isinstance(session_service_from_env(), NasikoSessionService)

    def test_a_url_without_a_credential_fails_at_startup(self, monkeypatch):
        monkeypatch.setenv("NASIKO_STATE_URL", BASE_URL)

        with pytest.raises(NasikoStateError, match="NASIKO_AGENT_TOKEN"):
            session_service_from_env()


async def test_serialization_is_plain_json(service, state_server):
    session = await new_session(service)
    await service.append_event(session, text_event("hi", delta={"n": 1}))

    json.dumps(state_server.kv[f"adk/session/{APP}/u1/s1"].value["value"])  # must not raise


def _load_google_adk_agent():
    path = Path(__file__).resolve().parents[2] / "google-adk" / "src" / "agent.py"
    spec = importlib.util.spec_from_file_location("google_adk_agent_under_test", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


async def test_the_agent_survives_losing_the_session_create_race_to_another_replica(monkeypatch):
    module = _load_google_adk_agent()
    agent = module.ResearchAgent()
    created = []

    class RacedService:
        async def get_session(self, **_):
            return None

        async def create_session(self, **kwargs):
            created.append(kwargs)
            raise AlreadyExistsError("another replica created it")

    agent._session_service = RacedService()

    await agent._ensure_session("ctx-1")

    assert [c["session_id"] for c in created] == ["ctx-1"]
