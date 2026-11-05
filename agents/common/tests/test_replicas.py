"""Two replicas sharing one task: no write may land on top of a newer one it has not seen."""

from __future__ import annotations

import asyncio
import logging

import pytest
from fake_state_server import BASE_URL, TOKEN, FakeStateServer
from nasiko_state import NasikoStateClient, NasikoStateConflict, NasikoTaskStore
from sdk_compat import artifact_text, new_task, state_name, with_artifact_text, with_state

pytestmark = pytest.mark.anyio

LONG_WINDOW = 60  # deferred updates stay pending until the test flushes them


def task_puts(server: FakeStateServer, task_id: str = "t1") -> list:
    return [r for r in server.writes("tasks") if r.path == f"/tasks/{task_id}"]


def stored(server: FakeStateServer, task_id: str = "t1") -> tuple[str, str | None]:
    """The stored row's state and artifact text."""
    row = server.tasks[task_id].value
    artifacts = row["body"]["task"].get("artifacts")
    return row["state"], artifacts[0]["parts"][0]["text"] if artifacts else None


@pytest.fixture
async def replicas(state_server):
    """Two processes of one agent: separate clients and stores, one platform."""
    stores = [
        NasikoTaskStore(
            NasikoStateClient(BASE_URL, TOKEN, transport=state_server.transport, retry_backoff_seconds=0),
            coalesce_window_seconds=LONG_WINDOW,
        )
        for _ in range(2)
    ]
    yield stores
    for store in stores:
        await store.aclose()


async def hand_over(first: NasikoTaskStore, second: NasikoTaskStore) -> object:
    """The first replica pauses t1 with an unwritten update; the second resumes and completes it."""
    task = new_task("t1", "working", artifact_text="draft")
    await first.save(task)
    await first.save(with_state(task, "input-required"))
    await first.save(with_artifact_text(task, "stale"))  # coalesced: still pending on the first

    resumed = await second.get("t1")
    await second.save(with_artifact_text(with_state(resumed, "completed"), "final"))
    return task


class TestNoStaleOverwrite:
    async def test_a_stale_deferred_update_cannot_overwrite_another_replicas_newer_write(
        self, replicas, state_server
    ):
        first, second = replicas
        await hand_over(first, second)

        await first.aclose()  # flushes its pending update, conditional on the version it saw

        assert stored(state_server) == ("completed", "final")

    async def test_the_exit_flush_cannot_overwrite_it_either(self, replicas, state_server, caplog):
        first, second = replicas
        await hand_over(first, second)

        with caplog.at_level(logging.WARNING, logger="nasiko_state"):
            first.flush_sync()

        assert stored(state_server) == ("completed", "final")
        assert "changed by another writer" in caplog.text

    async def test_a_conflicting_state_change_raises_and_the_store_rereads(self, replicas, state_server):
        first, second = replicas
        task = await hand_over(first, second)

        with pytest.raises(NasikoStateConflict) as raised:
            await first.save(with_state(task, "failed"))

        assert raised.value.current_version == state_server.tasks["t1"].version
        assert stored(state_server) == ("completed", "final")
        assert artifact_text(await first.get("t1")) == "final"

    async def test_after_a_conflict_the_store_writes_on_top_of_what_it_reread(self, replicas, state_server):
        first, second = replicas
        task = await hand_over(first, second)
        with pytest.raises(NasikoStateConflict):
            await first.save(with_state(task, "failed"))

        await first.save(with_state(task, "failed"))  # the caller decided to overwrite after all

        assert stored(state_server)[0] == "failed"


class TestConditionalWrites:
    async def test_every_write_after_the_first_is_conditional_on_the_last_version(
        self, replicas, state_server
    ):
        store, _ = replicas
        task = new_task("t1", "working", artifact_text="1")
        await store.save(task)
        await store.save(with_state(task, "input-required"))
        await store.save(with_artifact_text(task, "2"))
        await store.aclose()

        assert [r.headers.get("if-match") for r in task_puts(state_server)] == [None, "1", "2"]

    async def test_a_task_read_from_the_server_is_written_conditionally_on_that_read(
        self, replicas, state_server
    ):
        first, second = replicas
        await first.save(new_task("t1", "input-required"))

        resumed = await second.get("t1")
        await second.save(with_state(resumed, "working"))

        assert task_puts(state_server)[-1].headers["if-match"] == "1"


class TestReads:
    async def test_get_returns_the_server_row_once_another_replica_wrote_past_the_local_update(
        self, replicas, state_server
    ):
        first, second = replicas
        await hand_over(first, second)

        seen = await first.get("t1")

        assert (state_name(seen), artifact_text(seen)) == ("completed", "final")
        await first.aclose()
        assert len(task_puts(state_server)) == 3  # the dropped update was never written

    async def test_get_returns_the_local_update_while_it_is_based_on_the_server_row(
        self, replicas, state_server
    ):
        store, _ = replicas
        task = new_task("t1", "working", artifact_text="1")
        await store.save(task)
        await store.save(with_artifact_text(task, "2"))

        assert artifact_text(await store.get("t1")) == "2"
        assert stored(state_server) == ("working", "1")

    async def test_a_get_that_finds_a_new_state_makes_a_return_to_the_old_one_a_state_change(
        self, replicas, state_server
    ):
        first, second = replicas
        task = new_task("t1", "working")
        await first.save(task)
        moved = await second.get("t1")
        await second.save(with_state(moved, "input-required"))

        reread = await first.get("t1")
        await first.save(with_state(reread, "working"))  # differs from the server, so written through

        assert stored(state_server)[0] == "working"


class TestWritesInFlight:
    async def test_a_save_in_the_state_already_being_written_does_not_wait_for_it(
        self, replicas, state_server
    ):
        store, _ = replicas
        task = new_task("t1", "working", artifact_text="1")
        await store.save(task)
        entered, release = asyncio.Event(), asyncio.Event()

        async def hold_the_next_put(request):
            if request.method == "PUT" and not entered.is_set():
                entered.set()
                await release.wait()

        state_server.gate = hold_the_next_put
        change = asyncio.create_task(store.save(with_state(task, "input-required")))
        await asyncio.wait_for(entered.wait(), 3)

        # Same state as the write in flight: coalesced, so it returns at once rather than queueing.
        await asyncio.wait_for(store.save(with_artifact_text(task, "2")), 1)

        release.set()
        await asyncio.wait_for(change, 3)
        await store.aclose()
        assert stored(state_server) == ("input-required", "2")
