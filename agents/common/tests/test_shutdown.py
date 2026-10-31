"""install_shutdown_hooks: pending task writes reach the platform when the server stops."""

from __future__ import annotations

import contextlib
import logging
import signal

import nasiko_state
import pytest
from nasiko_state import NasikoTaskStore, install_shutdown_hooks
from sdk_compat import new_task, with_artifact_text
from starlette.applications import Starlette

pytestmark = pytest.mark.anyio

LONG_WINDOW = 60  # nothing is written by timer; only the shutdown can flush


@pytest.fixture(autouse=True)
def isolated_hooks(monkeypatch):
    """Only this test's stores are open, and SIGTERM is back to its default afterwards."""
    monkeypatch.setattr(nasiko_state, "_open_task_stores", set())
    previous = signal.signal(signal.SIGTERM, signal.SIG_DFL)
    yield
    signal.signal(signal.SIGTERM, previous)


async def store_with_a_pending_update(client) -> NasikoTaskStore:
    store = NasikoTaskStore(client, coalesce_window_seconds=LONG_WINDOW)
    task = new_task("t1", "working", artifact_text="1")
    await store.save(task)
    await store.save(with_artifact_text(task, "2"))
    return store


def stored_text(server) -> str:
    return server.tasks["t1"].value["body"]["task"]["artifacts"][0]["parts"][0]["text"]


async def serve_and_stop(app) -> list[str]:
    """Run the application's lifespan the way an ASGI server does: start it, then shut it down."""
    incoming = iter([{"type": "lifespan.startup"}, {"type": "lifespan.shutdown"}])
    sent: list[str] = []

    async def receive():
        return next(incoming)

    async def send(message):
        sent.append(message["type"])

    await app({"type": "lifespan", "asgi": {"version": "3.0"}, "state": {}}, receive, send)
    return sent


class TestLifespan:
    async def test_server_shutdown_writes_what_is_pending(self, client, state_server):
        await store_with_a_pending_update(client)
        app = Starlette(routes=[])
        install_shutdown_hooks(app)
        assert stored_text(state_server) == "1"

        sent = await serve_and_stop(app)

        assert sent == ["lifespan.startup.complete", "lifespan.shutdown.complete"]
        assert stored_text(state_server) == "2"

    async def test_the_applications_own_lifespan_still_runs_first(self, client, state_server):
        await store_with_a_pending_update(client)
        events = []

        @contextlib.asynccontextmanager
        async def lifespan(app):
            events.append("started")
            yield
            events.append(f"stopped with {stored_text(state_server)!r} stored")

        app = Starlette(routes=[], lifespan=lifespan)
        install_shutdown_hooks(app)

        await serve_and_stop(app)

        assert events == ["started", "stopped with '1' stored"]
        assert stored_text(state_server) == "2"

    async def test_a_failed_flush_is_logged_and_shutdown_completes(self, client, state_server, caplog):
        await store_with_a_pending_update(client)
        state_server.fail(400, when=lambda r: r.method == "PUT")
        app = Starlette(routes=[])
        install_shutdown_hooks(app)

        with caplog.at_level(logging.ERROR, logger="nasiko_state"):
            sent = await serve_and_stop(app)

        assert sent[-1] == "lifespan.shutdown.complete"
        assert "could not be flushed at shutdown" in caplog.text

    async def test_a_closed_store_is_not_closed_again(self, client, state_server):
        store = await store_with_a_pending_update(client)
        await store.aclose()
        app = Starlette(routes=[])
        install_shutdown_hooks(app)
        requests_before = len(state_server.requests)

        await serve_and_stop(app)

        assert len(state_server.requests) == requests_before

    async def test_without_a_durable_store_shutdown_is_unchanged(self):
        app = Starlette(routes=[])
        install_shutdown_hooks(app)

        assert await serve_and_stop(app) == ["lifespan.startup.complete", "lifespan.shutdown.complete"]

    def test_a_wrapped_application_is_refused_with_an_explanation(self):
        class Middleware:
            def __init__(self, app):
                self.app = app

        with pytest.raises(TypeError, match="before wrapping"):
            install_shutdown_hooks(Middleware(Starlette(routes=[])))


class TestSigterm:
    def test_sigterm_becomes_a_normal_exit_so_the_atexit_flush_runs(self):
        install_shutdown_hooks(Starlette(routes=[]))
        handler = signal.getsignal(signal.SIGTERM)

        with pytest.raises(SystemExit) as raised:
            handler(signal.SIGTERM, None)

        assert raised.value.code == 128 + signal.SIGTERM

    def test_a_sigterm_handler_someone_else_installed_is_left_alone(self):
        def theirs(signum, frame):
            pass

        signal.signal(signal.SIGTERM, theirs)

        install_shutdown_hooks(Starlette(routes=[]))

        assert signal.getsignal(signal.SIGTERM) is theirs
