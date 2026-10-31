"""The factories: in-memory stores without the platform's state API, durable ones with it."""

from __future__ import annotations

import logging
import threading
import time

import httpx
import nasiko_state
import pytest
from a2a.server.tasks import InMemoryTaskStore
from fake_state_server import BASE_URL, TOKEN, FakeStateServer
from langgraph.checkpoint.memory import MemorySaver
from nasiko_state import (
    REGISTER_THREAD_NAME,
    RETRY_ATTEMPTS,
    NasikoCheckpointSaver,
    NasikoStateClient,
    NasikoStateError,
    NasikoTaskStore,
    make_checkpointer,
    make_task_store,
)
from sdk_compat import call_context, new_task

pytestmark = pytest.mark.anyio


@pytest.fixture
def no_network(monkeypatch):
    """Any HTTP request at all fails the test."""

    def refuse(self, *args, **kwargs):
        raise AssertionError("the state API was contacted")

    monkeypatch.setattr(httpx.Client, "request", refuse)
    monkeypatch.setattr(httpx.AsyncClient, "request", refuse)


@pytest.fixture
def platform(monkeypatch, state_server: FakeStateServer) -> FakeStateServer:
    """The platform's environment, with the factories' clients wired to the fake state server."""
    monkeypatch.setenv("NASIKO_STATE_URL", BASE_URL)
    monkeypatch.setenv("NASIKO_AGENT_TOKEN", TOKEN)
    monkeypatch.setattr(nasiko_state, "REGISTER_RETRY_FIRST_SECONDS", 0)

    def from_env(env=None) -> NasikoStateClient:
        return NasikoStateClient(BASE_URL, TOKEN, transport=state_server.transport, retry_backoff_seconds=0)

    monkeypatch.setattr(NasikoStateClient, "from_env", staticmethod(from_env))
    yield state_server
    wait_for_registration()


def wait_for_registration(timeout: float = 5.0) -> None:
    """Join the background registrations the factories started."""
    for thread in threading.enumerate():
        if thread.name == REGISTER_THREAD_NAME:
            thread.join(timeout)
            assert not thread.is_alive(), "a background registration is still running"


def register_requests(server: FakeStateServer) -> list:
    return [r for r in server.requests if r.path == "/register"]


class TestWithoutTheStateApi:
    def test_returns_the_sdk_in_memory_store(self, no_network):
        assert type(make_task_store()) is InMemoryTaskStore

    async def test_works_as_a_store_without_ever_contacting_the_platform(self, no_network):
        store = make_task_store()

        await store.save(new_task("t1", "working"), call_context())

        assert (await store.get("t1", call_context())).id == "t1"

    def test_a_blank_url_counts_as_unset(self, monkeypatch, no_network):
        monkeypatch.setenv("NASIKO_STATE_URL", "   ")

        assert type(make_task_store()) is InMemoryTaskStore

    def test_a_token_alone_changes_nothing(self, monkeypatch, no_network):
        monkeypatch.setenv("NASIKO_AGENT_TOKEN", TOKEN)

        assert type(make_task_store()) is InMemoryTaskStore


class TestWithTheStateApi:
    def test_returns_the_durable_store(self, platform):
        assert isinstance(make_task_store(), NasikoTaskStore)

    def test_the_agent_registers_at_startup_before_any_task(self, platform):
        make_task_store()
        wait_for_registration()

        assert [r["capabilities"] for r in platform.registrations] == [["tasks"]]
        assert [r.path for r in platform.requests] == ["/register"]

    def test_registering_never_holds_up_startup(self, platform, monkeypatch):
        released = threading.Event()
        serve = platform.handle

        def held(request: httpx.Request) -> httpx.Response:
            assert released.wait(5), "the test never released the registration"
            return serve(request)

        monkeypatch.setattr(platform.transport, "handler", held)

        started = time.monotonic()
        store = make_task_store()

        assert isinstance(store, NasikoTaskStore)
        assert time.monotonic() - started < 1  # returned while the platform still sits on the request
        assert platform.registrations == []
        released.set()
        wait_for_registration()
        assert [r["capabilities"] for r in platform.registrations] == [["tasks"]]

    def test_a_failed_registration_is_retried_in_the_background(self, platform):
        failures = 2 * RETRY_ATTEMPTS  # outlasts the client's own retries twice over
        platform.fail(503, times=failures, when=lambda r: r.path == "/register")

        make_task_store()
        wait_for_registration()

        assert [r["capabilities"] for r in platform.registrations] == [["tasks"]]
        assert len(register_requests(platform)) == failures + 1

    def test_a_refused_registration_is_logged_and_left_to_the_first_write(self, platform, caplog):
        platform.fail(401, when=lambda r: r.path == "/register")

        with caplog.at_level(logging.ERROR, logger="nasiko_state"):
            make_task_store()
            wait_for_registration()

        assert platform.registrations == []
        assert len(register_requests(platform)) == 1
        assert "first write will try again" in caplog.text

    def test_a_url_without_a_credential_fails_at_startup(self, monkeypatch):
        monkeypatch.setenv("NASIKO_STATE_URL", BASE_URL)

        with pytest.raises(NasikoStateError, match="credential"):
            make_task_store()

    def test_the_gateway_token_alone_is_not_a_state_credential(self, monkeypatch):
        monkeypatch.setenv("NASIKO_STATE_URL", BASE_URL)
        monkeypatch.setenv("MCP_GATEWAY_TOKEN", TOKEN)

        with pytest.raises(NasikoStateError, match="NASIKO_AGENT_TOKEN"):
            make_task_store()


class TestCheckpointerFactory:
    def test_without_the_state_api_it_is_langgraphs_memory_saver(self, no_network):
        assert type(make_checkpointer()) is MemorySaver

    def test_with_the_state_api_it_is_the_durable_saver(self, platform):
        assert isinstance(make_checkpointer(), NasikoCheckpointSaver)

    def test_it_registers_key_value_state_at_startup(self, platform):
        make_checkpointer()
        wait_for_registration()

        assert [r["capabilities"] for r in platform.registrations] == [["kv"]]

    def test_a_url_without_a_credential_fails_at_startup(self, monkeypatch):
        monkeypatch.setenv("NASIKO_STATE_URL", BASE_URL)

        with pytest.raises(NasikoStateError, match="credential"):
            make_checkpointer()
