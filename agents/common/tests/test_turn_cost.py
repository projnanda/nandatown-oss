"""What one turn of the stock langgraph agent costs in state API calls, task store and checkpointer together.

The platform rate-limits each agent (600 calls a minute by default, shared by all its replicas), so
the cost of a turn decides how many turns an agent can serve, and is part of the stores' contract.
The executor below mirrors oss/agents/langgraph: the SDK's request handler saves the task as it
goes (submitted, working, completed), while a ReAct graph with one tool call runs with
durability="exit" and the answer is taken from the last streamed state.
"""

from __future__ import annotations

import asyncio
from collections import Counter
from typing import Any

import pytest
from a2a.server.agent_execution import AgentExecutor, RequestContext
from a2a.server.events import EventQueue
from a2a.server.tasks import TaskUpdater
from fake_state_server import Recorded
from langchain_core.messages import AIMessage, ToolMessage
from nasiko_state import NasikoCheckpointSaver, NasikoKV, NasikoTaskStore
from sdk_compat import A2A_V1, state_name
from sdk_harness import WORKING, build_handler, finish, send, start_task, text_parts, user_message
from test_checkpointer import react_agent_with_one_tool_call

pytestmark = pytest.mark.anyio

WINDOW = 0.01  # short, so any coalesced write lands inside the turn being counted


class StockLanggraphTurn(AgentExecutor):
    def __init__(self, checkpointer: NasikoCheckpointSaver) -> None:
        self.checkpointer = checkpointer

    async def execute(self, context: RequestContext, event_queue: EventQueue) -> None:
        task = context.current_task
        if task is None:
            task = start_task(context.message)
            await event_queue.enqueue_event(task)
        updater = TaskUpdater(event_queue, task.id, task.context_id)
        graph = react_agent_with_one_tool_call(self.checkpointer)
        config = {"configurable": {"thread_id": task.context_id}}
        final_state: dict[str, Any] = {}
        inputs = {"messages": [("user", context.get_user_input())]}
        async for state in graph.astream(inputs, config, stream_mode="values", durability="exit"):
            final_state = state
            last = state["messages"][-1]
            if isinstance(last, ToolMessage) or (isinstance(last, AIMessage) and last.tool_calls):
                await updater.update_status(WORKING)
        await updater.add_artifact(text_parts(final_state["messages"][-1].content), name="analysis_result")
        await finish(updater)

    async def cancel(self, context: RequestContext, event_queue: EventQueue) -> None:
        raise NotImplementedError


def endpoint(request: Recorded) -> str:
    """A request's endpoint, with ids dropped and key-value keys reduced to their family."""
    kind, _, rest = request.path.removeprefix("/").partition("/")
    if kind == "kv" and rest:
        return f"{request.method} /kv/{rest.split('%3A')[0]}"
    if kind == "kv":
        return f"{request.method} /kv?prefix"
    if kind == "tasks" and rest:
        return f"{request.method} /tasks/{{id}}"
    return f"{request.method} /{kind}"


async def take_turns(state_server, client, turns: int) -> list[Counter]:
    """Run `turns` turns of one conversation and count each turn's calls, registration aside."""
    store = NasikoTaskStore(client, coalesce_window_seconds=WINDOW)
    handler = build_handler(StockLanggraphTurn(NasikoCheckpointSaver(NasikoKV(client))), store)
    context_id = None
    costs = []
    for turn in range(turns):
        state_server.requests.clear()
        result = await send(handler, user_message(f"question {turn}", context_id=context_id))
        await asyncio.sleep(WINDOW * 5)  # let any coalesced write land before counting
        assert state_name(result) == "completed"
        context_id = result.context_id
        costs.append(Counter(endpoint(r) for r in state_server.requests if r.path != "/register"))
    await store.aclose()
    return costs


# a2a-sdk 1.x looks a new task up before saving it; once, thanks to the store's miss cache.
TASK_LOOKUP = Counter({"GET /tasks/{id}": 1} if A2A_V1 else {})
# Status write-through: submitted, working and completed each reach the server as they happen.
TASK_WRITES = Counter({"PUT /tasks/{id}": 3})
CHECKPOINT_WRITE = Counter({"PUT /kv/lg": 1, "PUT /kv/lgh": 1})


async def test_the_first_turn_of_a_conversation(state_server, client):
    (first,) = await take_turns(state_server, client, 1)

    assert first == TASK_LOOKUP + TASK_WRITES + Counter({"GET /kv/lgh": 1}) + CHECKPOINT_WRITE


async def test_a_turn_with_history(state_server, client):
    _, second, third = await take_turns(state_server, client, 3)

    expected = TASK_LOOKUP + TASK_WRITES + Counter({"GET /kv/lgh": 1, "GET /kv?prefix": 1}) + CHECKPOINT_WRITE
    assert second == expected
    assert third == expected
    assert sum(second.values()) == (8 if A2A_V1 else 7)
