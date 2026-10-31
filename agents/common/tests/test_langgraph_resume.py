"""The stock langgraph agent's executor, resumed after an input-required pause.

The platform's HITL dispatcher delivers a human's answer as a new message on the paused taskId and
reads the reply stream to its first paused or final event. Re-enqueueing the stored task made the
SDK stream that task's old input-required snapshot first, so the answer looked like a fresh pause
and never reached the task history. The executor here is the real one from oss/agents/langgraph,
driven by the real SDK request handler; only its model is replaced by a scripted one.
"""

from __future__ import annotations

import importlib.util
import sys
import types
from pathlib import Path
from typing import Any

import pytest
from nasiko_state import NasikoStateClient, NasikoTaskStore
from sdk_compat import A2A_V1, call_context
from sdk_harness import build_handler, user_message

pytestmark = [
    pytest.mark.anyio,
    pytest.mark.skipif(not A2A_V1, reason="the stock langgraph agent runs on a2a-sdk 1.x"),
]

LANGGRAPH_SRC = Path(__file__).resolve().parents[2] / "langgraph" / "src"
QUESTION = "Which color should I use?"
ANSWER = "BLUE"


class ScriptedAnalyst:
    """Stands in for DeepAnalystAgent: asks a question on the first turn and answers the next one."""

    def __init__(self) -> None:
        self.queries: list[str] = []

    async def stream(self, query: str, context_id: str):
        self.queries.append(query)
        if len(self.queries) == 1:
            yield {"is_task_complete": False, "require_user_input": True, "content": QUESTION}
            return
        yield {"is_task_complete": True, "require_user_input": False, "content": f"Using {query}"}


def load_source(name: str) -> types.ModuleType:
    spec = importlib.util.spec_from_file_location(name, LANGGRAPH_SRC / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


@pytest.fixture
def executor(monkeypatch: pytest.MonkeyPatch) -> Any:
    """The stock executor, importing its own telemetry module and the scripted model as `agent`."""
    scripted_agent = types.ModuleType("agent")
    scripted_agent.DeepAnalystAgent = ScriptedAnalyst
    monkeypatch.setitem(sys.modules, "agent", scripted_agent)
    monkeypatch.setitem(sys.modules, "telemetry", load_source("telemetry"))
    return load_source("agent_executor").DeepAnalystAgentExecutor()


def task_store(kind: str, state_server) -> Any:
    if kind == "memory":
        from a2a.server.tasks import InMemoryTaskStore

        return InMemoryTaskStore()
    client = NasikoStateClient(
        "http://state.test/api/agent-state",
        "test-agent-token",
        transport=state_server.transport,
        retry_backoff_seconds=0,
    )
    return NasikoTaskStore(client)


async def stream_states(handler, message) -> list[str]:
    """The task state carried by each streamed event, in order; events without one are skipped."""
    from a2a.server.context import ServerCallContext
    from a2a.types import SendMessageRequest, TaskState

    states = []
    async for event in handler.on_message_send_stream(
        SendMessageRequest(message=message), ServerCallContext()
    ):
        status = getattr(event, "status", None)
        if status is not None:
            states.append(TaskState.Name(status.state))
    return states


async def pause(handler) -> tuple[str, str]:
    from a2a.server.context import ServerCallContext
    from a2a.types import SendMessageRequest, TaskState

    events = [
        event
        async for event in handler.on_message_send_stream(
            SendMessageRequest(message=user_message("ask me a color")), ServerCallContext()
        )
    ]
    paused = events[-1]
    assert paused.status.state == TaskState.TASK_STATE_INPUT_REQUIRED
    return paused.task_id, paused.context_id


@pytest.mark.parametrize("store_kind", ["memory", "platform"])
async def test_a_resumed_task_streams_the_answer_not_the_old_pause(executor, state_server, store_kind):
    store = task_store(store_kind, state_server)
    handler = build_handler(executor, store)
    task_id, context_id = await pause(handler)

    states = await stream_states(handler, user_message(ANSWER, task_id=task_id, context_id=context_id))

    # The dispatcher stops at the first paused or final state, so the old pause must not come first.
    assert states == ["TASK_STATE_COMPLETED"]
    assert executor.agent.queries == ["ask me a color", ANSWER]
    stored = await store.get(task_id, call_context())
    assert [part.text for message in stored.history for part in message.parts][-1] == ANSWER
    if store_kind == "platform":
        await store.aclose()
