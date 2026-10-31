"""How NasikoTaskStore batches writes: what is written through, what is deferred, in what order."""

from __future__ import annotations

import asyncio
import atexit
import logging
import time

import nasiko_state
import pytest
from fake_state_server import FakeStateServer
from nasiko_state import RETRY_ATTEMPTS, NasikoStateError, NasikoTaskStore
from sdk_compat import artifact_text, new_task, with_artifact_text, with_state

pytestmark = pytest.mark.anyio

WINDOW = 0.2
LONG_WINDOW = 60


def task_writes(server: FakeStateServer, task_id: str = "t1") -> list:
    return [r for r in server.writes("tasks") if r.path == f"/tasks/{task_id}"]


def written_texts(server: FakeStateServer, task_id: str = "t1") -> list[str | None]:
    """The artifact text of every PUT, in arrival order."""
    texts = []
    for request in task_writes(server, task_id):
        artifacts = request.json["body"]["task"].get("artifacts")
        texts.append(artifacts[0]["parts"][0]["text"] if artifacts else None)
    return texts


async def eventually(condition, timeout: float = 3.0) -> None:
    deadline = time.monotonic() + timeout
    while not condition():
        if time.monotonic() > deadline:
            raise AssertionError("condition not met in time")
        await asyncio.sleep(0.01)


@pytest.fixture
async def store(client):
    task_store = NasikoTaskStore(client, coalesce_window_seconds=WINDOW)
    yield task_store
    await task_store.aclose()


@pytest.fixture
async def default_store(client):
    """The production window, for the one test that states the 500 ms contract literally."""
    task_store = NasikoTaskStore(client)
    yield task_store
    await task_store.aclose()


@pytest.fixture
async def patient_store(client):
    """A window so long that no timer can fire during the test."""
    task_store = NasikoTaskStore(client, coalesce_window_seconds=LONG_WINDOW)
    yield task_store
    await task_store.aclose()


class TestWriteThrough:
    async def test_the_first_save_of_a_task_is_written_immediately(self, patient_store, state_server):
        await patient_store.save(new_task("t1", "submitted"))

        assert len(task_writes(state_server)) == 1

    async def test_input_required_is_on_the_server_before_save_returns(self, patient_store, state_server):
        task = new_task("t1", "working")
        await patient_store.save(task)

        await patient_store.save(with_state(task, "input-required"))

        assert state_server.tasks["t1"].value["state"] == "input-required"

    @pytest.mark.parametrize(
        "state", ["working", "input-required", "auth-required", "completed", "failed", "canceled", "rejected"]
    )
    async def test_every_state_change_is_written_before_save_returns(
        self, patient_store, state_server, state
    ):
        task = new_task("t1", "submitted")
        await patient_store.save(task)

        await patient_store.save(with_state(task, state))

        assert state_server.tasks["t1"].value["state"] == state
        assert len(task_writes(state_server)) == 2

    async def test_a_state_change_carries_the_updates_that_were_waiting(self, patient_store, state_server):
        task = new_task("t1", "working", artifact_text="1")
        await patient_store.save(task)
        await patient_store.save(with_artifact_text(task, "2"))
        await patient_store.save(with_artifact_text(task, "3"))

        await patient_store.save(with_state(task, "input-required"))

        assert written_texts(state_server) == ["1", "3"]


class TestCoalescing:
    async def test_three_updates_within_100ms_make_one_write(self, default_store, state_server):
        task = new_task("t1", "working", artifact_text="0")
        await default_store.save(task)  # first save: written through
        for text in ("1", "2", "3"):
            await default_store.save(with_artifact_text(task, text))
            await asyncio.sleep(0.03)

        assert len(task_writes(state_server)) == 1  # nothing deferred has landed yet
        await eventually(lambda: len(task_writes(state_server)) == 2)
        await asyncio.sleep(0.6)

        assert written_texts(state_server) == ["0", "3"]  # one write, carrying the newest content

    async def test_writes_are_at_most_one_per_window(self, store, state_server):
        task = new_task("t1", "working", artifact_text="0")
        started = time.monotonic()
        await store.save(task)
        for n in range(1, 61):
            await store.save(with_artifact_text(task, str(n)))
            await asyncio.sleep(0.02)
        elapsed = time.monotonic() - started
        await eventually(lambda: written_texts(state_server)[-1] == "60")

        writes = len(task_writes(state_server))

        assert writes <= elapsed / WINDOW + 2
        assert writes >= 3  # it did keep flushing while updates kept coming

    async def test_the_newest_update_always_lands(self, store, state_server):
        task = new_task("t1", "working", artifact_text="0")
        await store.save(task)
        for n in range(1, 8):
            await store.save(with_artifact_text(task, str(n)))

        await eventually(lambda: written_texts(state_server)[-1] == "7")

    async def test_an_idle_task_gets_its_next_update_after_at_most_one_window(self, store, state_server):
        task = new_task("t1", "working", artifact_text="0")
        await store.save(task)
        await asyncio.sleep(WINDOW * 2)  # the previous write is long outside the window

        started = time.monotonic()
        await store.save(with_artifact_text(task, "1"))
        await eventually(lambda: written_texts(state_server)[-1] == "1")

        assert time.monotonic() - started < WINDOW  # due immediately, not a full window later

    async def test_tasks_are_throttled_independently(self, store, state_server):
        for task_id in ("a", "b"):
            task = new_task(task_id, "working", artifact_text="0")
            await store.save(task)
            await store.save(with_artifact_text(task, "1"))

        await eventually(lambda: written_texts(state_server, "a")[-1] == "1")
        await eventually(lambda: written_texts(state_server, "b")[-1] == "1")


class TestTerminalStates:
    async def test_completion_flushes_pending_updates_in_a_single_write(self, patient_store, state_server):
        task = new_task("t1", "working", artifact_text="1")
        await patient_store.save(task)
        await patient_store.save(with_artifact_text(task, "2"))
        await patient_store.save(with_artifact_text(task, "3"))

        await patient_store.save(with_state(task, "completed"))
        await asyncio.sleep(0.05)

        assert state_server.tasks["t1"].value["state"] == "completed"
        assert written_texts(state_server) == ["1", "3"]  # nothing stale is written afterwards

    async def test_a_finished_task_owes_nothing_and_its_slot_can_be_evicted(
        self, patient_store, state_server
    ):
        task = new_task("t1", "working")
        await patient_store.save(task)
        await patient_store.save(with_state(task, "completed"))

        assert patient_store._slots["t1"].is_idle()

    async def test_a_finished_task_still_reads_back(self, patient_store):
        task = new_task("t1", "working", artifact_text="done")
        await patient_store.save(task)
        await patient_store.save(with_state(task, "completed"))

        assert artifact_text(await patient_store.get("t1")) == "done"


class TestOrdering:
    async def test_writes_arrive_in_save_order(self, patient_store, state_server):
        task = new_task("t1", "working", artifact_text="1")
        await patient_store.save(task)
        for text in ("2", "3"):
            await patient_store.save(with_artifact_text(task, text))
        await patient_store.save(with_artifact_text(with_state(task, "input-required"), "4"))
        for text in ("5", "6"):
            await patient_store.save(with_artifact_text(task, text))
        await patient_store.aclose()

        assert written_texts(state_server) == ["1", "4", "6"]
        assert state_server.tasks["t1"].value["state"] == "input-required"

    async def test_a_slow_deferred_write_cannot_land_after_a_newer_state_change(self, client, state_server):
        store = NasikoTaskStore(client, coalesce_window_seconds=0.05)
        task = new_task("t1", "working", artifact_text="1")
        await store.save(task)

        entered, release = asyncio.Event(), asyncio.Event()
        held = []

        async def hold_the_next_put(request):
            if request.method == "PUT" and not held:
                held.append(request)
                entered.set()
                await release.wait()

        state_server.gate = hold_the_next_put
        # Saves run as tasks so a regression shows up as a failed assertion, never as a hung test.
        deferred = asyncio.create_task(store.save(with_artifact_text(task, "2")))
        await asyncio.wait_for(entered.wait(), 3)  # the write of "2" is now in flight and held
        change = asyncio.create_task(store.save(with_artifact_text(with_state(task, "input-required"), "3")))
        await asyncio.sleep(0.1)
        release.set()
        await asyncio.wait_for(asyncio.gather(deferred, change), 5)
        await store.aclose()

        assert written_texts(state_server) == ["1", "2", "3"]
        assert state_server.tasks["t1"].value["state"] == "input-required"


class TestReads:
    async def test_get_sees_this_processs_unwritten_update(self, patient_store, state_server):
        task = new_task("t1", "working", artifact_text="1")
        await patient_store.save(task)
        await patient_store.save(with_artifact_text(task, "2"))

        assert artifact_text(await patient_store.get("t1")) == "2"
        assert written_texts(state_server) == ["1"]  # the server has not seen it yet


class TestShutdown:
    async def test_aclose_writes_what_is_pending(self, client, state_server):
        store = NasikoTaskStore(client, coalesce_window_seconds=LONG_WINDOW)
        task = new_task("t1", "working", artifact_text="1")
        await store.save(task)
        await store.save(with_artifact_text(task, "2"))

        await store.aclose()

        assert written_texts(state_server) == ["1", "2"]

    async def test_aclose_with_a_failing_server_raises_instead_of_dropping_silently(
        self, client, state_server
    ):
        store = NasikoTaskStore(client, coalesce_window_seconds=LONG_WINDOW)
        task = new_task("t1", "working", artifact_text="1")
        await store.save(task)
        await store.save(with_artifact_text(task, "2"))
        state_server.fail(503, times=RETRY_ATTEMPTS)

        with pytest.raises(NasikoStateError):
            await store.aclose()

    async def test_the_exit_hook_flushes_through_the_sync_client(self, client, state_server, monkeypatch):
        registered = []
        monkeypatch.setattr(atexit, "register", lambda function: registered.append(function))
        store = NasikoTaskStore(client, coalesce_window_seconds=LONG_WINDOW)
        task = new_task("t1", "working", artifact_text="1")
        await store.save(task)
        await store.save(with_artifact_text(task, "2"))
        assert written_texts(state_server) == ["1"]

        assert len(registered) == 1
        registered[0]()  # what the interpreter does at exit, after the event loop is gone

        assert written_texts(state_server) == ["1", "2"]
        registered[0]()  # idempotent: nothing is written twice
        assert written_texts(state_server) == ["1", "2"]

    async def test_aclose_deregisters_the_exit_hook(self, client, monkeypatch):
        unregistered = []
        monkeypatch.setattr(atexit, "unregister", lambda function: unregistered.append(function))
        store = NasikoTaskStore(client)

        await store.aclose()

        assert unregistered == [store.flush_sync]

    async def test_the_exit_flush_gives_up_when_its_budget_is_spent(
        self, client, state_server, monkeypatch, caplog
    ):
        store = NasikoTaskStore(client, coalesce_window_seconds=LONG_WINDOW)
        task = new_task("t1", "working", artifact_text="1")
        await store.save(task)
        await store.save(with_artifact_text(task, "2"))
        monkeypatch.setattr(nasiko_state, "EXIT_FLUSH_BUDGET_SECONDS", -1)

        with caplog.at_level(logging.ERROR, logger="nasiko_state"):
            store.flush_sync()

        assert "budget" in caplog.text
        assert written_texts(state_server) == ["1"]
        atexit.unregister(store.flush_sync)
        await store.aclose()

    async def test_a_failed_exit_flush_is_logged_and_the_rest_still_written(
        self, client, state_server, caplog
    ):
        store = NasikoTaskStore(client, coalesce_window_seconds=LONG_WINDOW)
        for task_id in ("a", "b"):
            task = new_task(task_id, "working", artifact_text="1")
            await store.save(task)
            await store.save(with_artifact_text(task, "2"))
        state_server.fail(413, when=lambda r: r.method == "PUT" and r.path == "/tasks/a")

        with caplog.at_level(logging.ERROR, logger="nasiko_state"):
            store.flush_sync()

        assert "exit flush of task a failed" in caplog.text
        assert written_texts(state_server, "b") == ["1", "2"]
        atexit.unregister(store.flush_sync)

    async def test_a_hard_crash_loses_only_the_unflushed_non_status_updates(self, client, state_server):
        store = NasikoTaskStore(client, coalesce_window_seconds=LONG_WINDOW)
        task = new_task("t1", "working", artifact_text="1")
        await store.save(task)
        await store.save(with_state(task, "input-required"))  # a state change: durable at once
        await store.save(with_artifact_text(task, "2"))  # inside the window when the process dies
        atexit.unregister(store.flush_sync)  # a crash never runs exit hooks
        store._slots["t1"].flusher.cancel()

        assert state_server.tasks["t1"].value["state"] == "input-required"
        assert written_texts(state_server) == ["1", "1"]


class TestDeferredFailures:
    async def test_a_failed_deferred_write_is_logged_and_retried_without_another_save(
        self, store, state_server, caplog
    ):
        task = new_task("t1", "working", artifact_text="0")
        await store.save(task)
        state_server.fail(500, times=RETRY_ATTEMPTS, when=lambda r: r.method == "PUT")
        with caplog.at_level(logging.ERROR, logger="nasiko_state"):
            await store.save(with_artifact_text(task, "1"))
            await eventually(lambda: "deferred write of task t1 failed (will retry)" in caplog.text)

        await eventually(lambda: written_texts(state_server)[-1] == "1")

    async def test_a_failed_state_change_is_retried_in_the_background(self, store, state_server):
        task = new_task("t1", "working")
        await store.save(task)
        state_server.fail(503, times=RETRY_ATTEMPTS, when=lambda r: r.method == "PUT")
        with pytest.raises(NasikoStateError):
            await store.save(with_state(task, "input-required"))

        await eventually(lambda: state_server.tasks["t1"].value["state"] == "input-required")

    async def test_background_retries_back_off(self, client, state_server):
        store = NasikoTaskStore(client, coalesce_window_seconds=WINDOW)
        task = new_task("t1", "working", artifact_text="0")
        await store.save(task)
        state_server.fail(500, times=10_000, when=lambda r: r.method == "PUT")
        await store.save(with_artifact_text(task, "1"))

        await asyncio.sleep(WINDOW * 8)

        # A retry every window would be 8 deferred writes by now; doubling (0.2, 0.4, 0.8 s) is 3.
        assert len(task_writes(state_server)) - 1 <= 3 * RETRY_ATTEMPTS
        with pytest.raises(NasikoStateError):
            await store.aclose()

    async def test_retrying_stops_once_the_update_is_too_old_to_trust(
        self, store, state_server, monkeypatch, caplog
    ):
        monkeypatch.setattr(nasiko_state, "PENDING_MAX_AGE_SECONDS", WINDOW * 3)
        task = new_task("t1", "working", artifact_text="0")
        await store.save(task)
        state_server.fail(500, times=10_000, when=lambda r: r.method == "PUT")
        with caplog.at_level(logging.ERROR, logger="nasiko_state"):
            await store.save(with_artifact_text(task, "1"))
            await eventually(lambda: "discarding an update" in caplog.text)

        await eventually(lambda: store._slots["t1"].is_idle())
        requests_when_discarded = len(state_server.requests)
        await asyncio.sleep(WINDOW * 3)
        assert len(state_server.requests) == requests_when_discarded  # no retry after the discard

    async def test_a_rejected_deferred_write_is_dropped_not_retried(self, store, state_server, caplog):
        task = new_task("t1", "working", artifact_text="0")
        await store.save(task)
        state_server.fail(413, when=lambda r: r.method == "PUT")
        with caplog.at_level(logging.ERROR, logger="nasiko_state"):
            await store.save(with_artifact_text(task, "x" * 10))
            await eventually(lambda: "(dropped)" in caplog.text)

        await asyncio.sleep(WINDOW * 3)
        assert len(task_writes(state_server)) == 2  # the first save and the one refused write

    async def test_delete_discards_a_pending_update_so_it_cannot_resurrect_the_task(
        self, patient_store, state_server
    ):
        task = new_task("t1", "working", artifact_text="1")
        await patient_store.save(task)
        await patient_store.save(with_artifact_text(task, "2"))

        await patient_store.delete("t1")
        await patient_store.aclose()

        assert "t1" not in state_server.tasks


class TestBoundedMemory:
    async def test_idle_slots_are_evicted_beyond_the_cap(self, client, state_server, monkeypatch):
        monkeypatch.setattr(nasiko_state, "MAX_TRACKED_TASKS", 3)
        store = NasikoTaskStore(client, coalesce_window_seconds=LONG_WINDOW)
        for n in range(6):
            await store.save(new_task(f"t{n}", "working"))

        assert len(store._slots) == 3
        assert list(store._slots) == ["t3", "t4", "t5"]  # the least recently used went first
        await store.aclose()

    async def test_a_forgotten_task_costs_one_extra_write_not_correctness(
        self, client, state_server, monkeypatch
    ):
        monkeypatch.setattr(nasiko_state, "MAX_TRACKED_TASKS", 1)
        store = NasikoTaskStore(client, coalesce_window_seconds=LONG_WINDOW)
        first = new_task("t1", "working", artifact_text="1")
        await store.save(first)
        await store.save(new_task("t2", "working"))  # evicts t1's slot

        await store.save(with_artifact_text(first, "2"))  # unknown again, so written through

        assert written_texts(state_server) == ["1", "2"]
        await store.aclose()

    async def test_slots_owing_a_write_are_never_evicted(self, client, state_server, monkeypatch):
        monkeypatch.setattr(nasiko_state, "MAX_TRACKED_TASKS", 1)
        store = NasikoTaskStore(client, coalesce_window_seconds=LONG_WINDOW)
        first = new_task("t1", "working", artifact_text="1")
        await store.save(first)
        await store.save(with_artifact_text(first, "2"))  # pending
        await store.save(new_task("t2", "working"))

        assert list(store._slots) == ["t1", "t2"]  # over the cap rather than losing a pending write
        await store.aclose()
        assert written_texts(state_server) == ["1", "2"]


class TestExitBudget:
    def test_the_budget_bounds_each_request_not_only_the_loop_between_tasks(self, monkeypatch, caplog):
        """A slow, failing platform: the flush gives up inside a task's retries, on time."""
        import httpx
        from fake_state_server import BASE_URL, TOKEN
        from nasiko_state import NasikoStateClient

        server = FakeStateServer()
        attempts = []

        def slow_and_failing(request):
            if request.method == "PUT":
                attempts.append(request.extensions["timeout"]["read"])
                time.sleep(0.2)
                return httpx.Response(503, json={"error": "unavailable"})
            return server.handle(request)

        client = NasikoStateClient(
            BASE_URL, TOKEN, transport=httpx.MockTransport(slow_and_failing), retry_backoff_seconds=0
        )
        store = NasikoTaskStore(client, coalesce_window_seconds=LONG_WINDOW)
        for task_id in ("a", "b"):
            store._slot(task_id).pending = nasiko_state._snapshot(new_task(task_id, "working"))
        monkeypatch.setattr(nasiko_state, "EXIT_FLUSH_BUDGET_SECONDS", 0.3)

        started = time.monotonic()
        with caplog.at_level(logging.ERROR, logger="nasiko_state"):
            store.flush_sync()
        elapsed = time.monotonic() - started
        atexit.unregister(store.flush_sync)

        assert len(attempts) == 2  # task a: the second attempt is the last that fits; no third
        assert attempts[1] <= 0.15  # and it was only given the time left
        assert "budget spent; task b" in caplog.text
        assert elapsed < 0.6
