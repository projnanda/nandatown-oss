"""NasikoTaskStore under the real a2a-sdk request handler, which is what agents actually run."""

from __future__ import annotations

import pytest
from fake_state_server import FakeStateServer
from nasiko_state import NasikoStateClient, NasikoTaskStore
from sdk_compat import artifact_texts, call_context, state_name
from sdk_harness import AskThenAnswer, StreamChunks, build_handler, send, user_message

pytestmark = pytest.mark.anyio


def replica(server: FakeStateServer, executor, **store_options) -> tuple:
    """A fresh agent process: its own client, store and handler, sharing only the platform's state."""
    client = NasikoStateClient(
        "http://state.test/api/agent-state",
        "test-agent-token",
        transport=server.transport,
        retry_backoff_seconds=0,
    )
    store = NasikoTaskStore(client, **store_options)
    return store, build_handler(executor, store)


async def test_a_paused_task_survives_a_restart_and_resumes(state_server):
    first_store, first_handler = replica(state_server, AskThenAnswer())
    paused = await send(first_handler, user_message("start"))

    assert state_name(paused) == "input-required"
    assert state_server.tasks[paused.id].value["state"] == "input-required"  # durable before the replica dies
    await first_store.aclose()  # the replica is gone; only the platform's state remains

    second_store, second_handler = replica(state_server, AskThenAnswer())
    done = await send(
        second_handler, user_message("acme/api", task_id=paused.id, context_id=paused.context_id)
    )

    assert state_name(done) == "completed"
    assert state_server.tasks[paused.id].value["state"] == "completed"
    stored = await second_store.get(paused.id, call_context())
    assert artifact_texts(stored) == ["draft", "answer: acme/api"]  # the pre-restart work was not lost
    await second_store.aclose()


async def test_a_second_replica_sees_a_task_the_first_one_started(state_server):
    first_store, first_handler = replica(state_server, AskThenAnswer())
    paused = await send(first_handler, user_message("start"))

    other_store, other_handler = replica(state_server, AskThenAnswer())
    done = await send(
        other_handler, user_message("elsewhere", task_id=paused.id, context_id=paused.context_id)
    )

    assert state_name(done) == "completed"  # the first replica is still alive and untouched
    await first_store.aclose()
    await other_store.aclose()


async def test_streamed_chunks_are_coalesced_into_a_few_writes(state_server):
    store, handler = replica(state_server, StreamChunks(chunks=40))

    task = await send(handler, user_message("go"))
    await store.aclose()

    writes = [r for r in state_server.writes("tasks") if r.path == f"/tasks/{task.id}"]
    final_text = state_server.tasks[task.id].value["body"]["task"]["artifacts"][0]["parts"]
    assert state_server.tasks[task.id].value["state"] == "completed"
    assert "".join(part["text"] for part in final_text) == "".join(f"chunk{n} " for n in range(40))
    assert len(writes) <= 6  # submitted, working, completed, and at most a couple of flushes between
