"""Drive a real a2a-sdk DefaultRequestHandler without HTTP, under either SDK major.

The two majors differ in how messages are built, how the handler is constructed and called, and
how a task is paused or finished, so the integration tests speak through these few functions.
"""

from __future__ import annotations

import uuid
from typing import Any

from a2a.server.agent_execution import AgentExecutor, RequestContext
from a2a.server.events import EventQueue
from a2a.server.request_handlers import DefaultRequestHandler
from a2a.server.tasks import TaskUpdater
from sdk_compat import A2A_V1

if A2A_V1:
    from a2a.helpers import new_task_from_user_message, new_text_message
    from a2a.server.context import ServerCallContext
    from a2a.types import (
        AgentCapabilities,
        AgentCard,
        AgentInterface,
        Message,
        Part,
        Role,
        SendMessageRequest,
        TaskState,
    )

    PAUSED = TaskState.TASK_STATE_INPUT_REQUIRED
    WORKING = TaskState.TASK_STATE_WORKING
    COMPLETED = TaskState.TASK_STATE_COMPLETED

    def build_handler(executor: AgentExecutor, store: Any) -> DefaultRequestHandler:
        card = AgentCard(
            name="harness",
            description="test agent",
            version="1",
            supported_interfaces=[AgentInterface(protocol_binding="JSONRPC", url="http://localhost/")],
            capabilities=AgentCapabilities(streaming=True),
            default_input_modes=["text"],
            default_output_modes=["text"],
        )
        return DefaultRequestHandler(agent_executor=executor, task_store=store, agent_card=card)

    def user_message(text: str, *, task_id: str | None = None, context_id: str | None = None) -> Any:
        message = Message(message_id=str(uuid.uuid4()), role=Role.ROLE_USER, parts=[Part(text=text)])
        if task_id:
            message.task_id = task_id
        if context_id:
            message.context_id = context_id
        return message

    async def send(handler: DefaultRequestHandler, message: Any) -> Any:
        return await handler.on_message_send(SendMessageRequest(message=message), ServerCallContext())

    def start_task(message: Any) -> Any:
        return new_task_from_user_message(message)

    def text_parts(text: str) -> list[Any]:
        return [Part(text=text)]

    async def pause(updater: TaskUpdater, question: str) -> None:
        await updater.update_status(PAUSED, new_text_message(question))

    async def finish(updater: TaskUpdater) -> None:
        await updater.update_status(COMPLETED)

else:
    from a2a.types import Message, MessageSendParams, Part, Role, TaskState, TextPart
    from a2a.utils import new_agent_text_message, new_task

    PAUSED = TaskState.input_required
    WORKING = TaskState.working
    COMPLETED = TaskState.completed

    def build_handler(executor: AgentExecutor, store: Any) -> DefaultRequestHandler:
        return DefaultRequestHandler(agent_executor=executor, task_store=store)

    def user_message(text: str, *, task_id: str | None = None, context_id: str | None = None) -> Any:
        return Message(
            message_id=str(uuid.uuid4()),
            role=Role.user,
            parts=[Part(root=TextPart(text=text))],
            task_id=task_id,
            context_id=context_id,
        )

    async def send(handler: DefaultRequestHandler, message: Any) -> Any:
        return await handler.on_message_send(MessageSendParams(message=message))

    def start_task(message: Any) -> Any:
        return new_task(message)

    def text_parts(text: str) -> list[Any]:
        return [Part(root=TextPart(text=text))]

    async def pause(updater: TaskUpdater, question: str) -> None:
        await updater.update_status(PAUSED, new_agent_text_message(question), final=True)

    async def finish(updater: TaskUpdater) -> None:
        await updater.update_status(COMPLETED, final=True)


class AskThenAnswer(AgentExecutor):
    """Pauses for input on a new task, and answers when the same task is resumed."""

    async def execute(self, context: RequestContext, event_queue: EventQueue) -> None:
        resuming = context.current_task is not None
        task = context.current_task or start_task(context.message)
        if not resuming:
            await event_queue.enqueue_event(task)
        updater = TaskUpdater(event_queue, task.id, task.context_id)
        await updater.update_status(WORKING)
        if resuming:
            await updater.add_artifact(text_parts("answer: " + context.get_user_input()), name="answer")
            await finish(updater)
        else:
            await updater.add_artifact(text_parts("draft"), name="draft")
            await pause(updater, "Which repo?")

    async def cancel(self, context: RequestContext, event_queue: EventQueue) -> None:
        raise NotImplementedError


class StreamChunks(AgentExecutor):
    """Streams many artifact chunks in one go, the way an LLM-backed agent streams tokens."""

    def __init__(self, chunks: int) -> None:
        self.chunks = chunks

    async def execute(self, context: RequestContext, event_queue: EventQueue) -> None:
        task = start_task(context.message)
        await event_queue.enqueue_event(task)
        updater = TaskUpdater(event_queue, task.id, task.context_id)
        await updater.update_status(WORKING)
        for index in range(self.chunks):
            await updater.add_artifact(
                text_parts(f"chunk{index} "), artifact_id="stream", name="stream", append=index > 0
            )
        await finish(updater)

    async def cancel(self, context: RequestContext, event_queue: EventQueue) -> None:
        raise NotImplementedError
