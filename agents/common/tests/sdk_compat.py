"""Build and inspect A2A tasks without caring which a2a-sdk major is installed.

0.3.x tasks are pydantic models and 1.x tasks are protobuf messages, so every test that needs a
task goes through these helpers and speaks in the platform's canonical state names
("working", "input-required", "completed", ...).
"""

from __future__ import annotations

from typing import Any

from a2a.types import Task, TaskState

try:
    from a2a.types import a2a_pb2  # a2a-sdk 1.x

    A2A_V1 = True
except ImportError:
    a2a_pb2 = None  # a2a-sdk 0.3.x
    A2A_V1 = False


def new_task(
    task_id: str, state: str, *, context_id: str = "ctx-1", artifact_text: str | None = None
) -> Task:
    status = _status(state)
    artifacts = [_artifact(artifact_text)] if artifact_text is not None else None
    if A2A_V1:
        task = Task(id=task_id, context_id=context_id, status=status)
        if artifacts:
            task.artifacts.extend(artifacts)
        return task
    return Task(id=task_id, context_id=context_id, status=status, artifacts=artifacts)


def with_state(task: Task, state: str) -> Task:
    """The same task moved to another state, as the SDK's task manager does it (in place)."""
    if A2A_V1:
        task.status.CopyFrom(_status(state))
    else:
        task.status = _status(state)
    return task


def with_artifact_text(task: Task, text: str) -> Task:
    """The same task with its artifact text replaced, state untouched."""
    artifact = _artifact(text)
    if A2A_V1:
        del task.artifacts[:]
        task.artifacts.append(artifact)
    else:
        task.artifacts = [artifact]
    return task


def artifact_texts(task: Task) -> list[str]:
    """The text of the first part of every artifact, in order."""
    return [_part_text(artifact.parts[0]) for artifact in task.artifacts or []]


def artifact_text(task: Task) -> str | None:
    texts = artifact_texts(task)
    return texts[0] if texts else None


def call_context() -> Any:
    """What the SDK hands every store call: a ServerCallContext on 1.x, nothing on 0.3.x."""
    if not A2A_V1:
        return None
    from a2a.server.context import ServerCallContext

    return ServerCallContext()


def state_name(task: Task) -> str:
    if A2A_V1:
        return a2a_pb2.TaskState.Name(task.status.state).removeprefix("TASK_STATE_").lower().replace("_", "-")
    return task.status.state.value


def _part_text(part: Any) -> str:
    return part.text if A2A_V1 else part.root.text


def _status(state: str) -> Any:
    from a2a.types import TaskStatus

    if A2A_V1:
        return TaskStatus(state=getattr(TaskState, "TASK_STATE_" + state.upper().replace("-", "_")))
    return TaskStatus(state=TaskState(state))


def _artifact(text: str) -> Any:
    from a2a.types import Artifact, Part

    if A2A_V1:
        return Artifact(artifact_id="artifact-1", parts=[Part(text=text)])
    from a2a.types import TextPart

    return Artifact(artifact_id="artifact-1", parts=[Part(root=TextPart(text=text))])
