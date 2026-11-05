"""Every a2a-1.x stock agent, resumed on a task the platform left input-required.

Re-enqueueing the stored task on resume makes a2a-sdk 1.x stream its old input-required snapshot
as the reply's first event, which the platform's HITL dispatcher reads as a brand new pause (see
test_langgraph_resume.py for the full story). None of these agents pauses itself, so the stored
pause is seeded directly. Each executor is imported from its real source; third-party SDKs the
test environment lacks (openai, opentelemetry, click, ...) are replaced by mocks, and an agent
whose module still cannot be imported is skipped with the reason printed.
"""

from __future__ import annotations

import importlib.util
import inspect
import sys
import types
from pathlib import Path
from unittest.mock import MagicMock

import pytest
from sdk_compat import A2A_V1, call_context, new_task
from sdk_harness import build_handler, user_message

pytestmark = [
    pytest.mark.anyio,
    pytest.mark.skipif(not A2A_V1, reason="the replay only happens on a2a-sdk 1.x"),
]

AGENTS = Path(__file__).resolve().parents[2]
EXECUTOR_SOURCES = [
    "openai/src/agent_executor.py",
    "translator/src/agent_executor.py",
    "summarizer/src/agent_executor.py",
    "test-agent/src/main.py",
    "currency-agent/main.py",
    "doc-reader/main.py",
    "assistant-agent/main.py",
    "simulated-agent/main.py",
]
MAX_STUBBED_IMPORTS = 25


def load_with_stubbed_sdks(source: Path, monkeypatch: pytest.MonkeyPatch) -> types.ModuleType:
    """Executes the agent module, mocking each third-party module the environment cannot import."""
    monkeypatch.syspath_prepend(str(source.parent))
    # Agents share sibling module names (agent, telemetry, toolset); forget another agent's copy.
    for sibling in ("agent", "telemetry", "toolset"):
        monkeypatch.delitem(sys.modules, sibling, raising=False)
    for _ in range(MAX_STUBBED_IMPORTS):
        spec = importlib.util.spec_from_file_location(f"stock_{source.parent.name}", source)
        module = importlib.util.module_from_spec(spec)
        try:
            spec.loader.exec_module(module)
            return module
        except ModuleNotFoundError as error:
            missing = error.name
            if missing is None or (source.parent / f"{missing}.py").exists():
                raise
            monkeypatch.setitem(sys.modules, missing, MagicMock(name=missing))
            # A mocked package must also answer to `import pkg.sub` for every sub it is asked for.
            parent, _, _ = missing.rpartition(".")
            if parent and parent not in sys.modules:
                monkeypatch.setitem(sys.modules, parent, MagicMock(name=parent))
    raise ImportError(f"{source} still has unresolved imports after {MAX_STUBBED_IMPORTS} stubs")


def executor_class(module: types.ModuleType) -> type:
    from a2a.server.agent_execution import AgentExecutor

    classes = [
        value
        for value in vars(module).values()
        if isinstance(value, type) and issubclass(value, AgentExecutor) and value.__module__ == module.__name__
    ]
    assert len(classes) == 1, f"expected one executor in {module.__name__}, found {classes}"
    return classes[0]


def build_executor(executor_type: type):
    """Instantiates the executor, handing a mock to each constructor argument (an LLM client, a model)."""
    required = [
        name
        for name, parameter in inspect.signature(executor_type).parameters.items()
        if parameter.default is inspect.Parameter.empty
    ]
    return executor_type(**{name: MagicMock(name=name) for name in required})


async def first_state_of_resume(handler, task_id: str, context_id: str) -> str:
    from a2a.server.context import ServerCallContext
    from a2a.types import SendMessageRequest, TaskState

    request = SendMessageRequest(message=user_message("my answer", task_id=task_id, context_id=context_id))
    async for event in handler.on_message_send_stream(request, ServerCallContext()):
        status = getattr(event, "status", None)
        if status is not None:
            return TaskState.Name(status.state)
    return "no status event"


@pytest.mark.parametrize("relative_source", EXECUTOR_SOURCES)
async def test_a_resumed_task_does_not_replay_its_old_pause(relative_source, monkeypatch):
    from a2a.server.tasks import InMemoryTaskStore

    try:
        module = load_with_stubbed_sdks(AGENTS / relative_source, monkeypatch)
        executor = build_executor(executor_class(module))
    except Exception as error:  # noqa: BLE001 - the skip reason is printed, never silently dropped
        reason = f"{relative_source} cannot be imported without its cloud SDK: {error!r}"
        print(reason)
        pytest.skip(reason)

    store = InMemoryTaskStore()
    await store.save(new_task("paused-task", "input-required", context_id="paused-ctx"), call_context())

    first = await first_state_of_resume(build_handler(executor, store), "paused-task", "paused-ctx")

    assert first != "TASK_STATE_INPUT_REQUIRED"


@pytest.mark.parametrize("relative_source", EXECUTOR_SOURCES)
def test_the_executor_enqueues_only_a_new_task(relative_source):
    """Source-level guard that holds even where the executor cannot be imported."""
    import ast

    tree = ast.parse((AGENTS / relative_source).read_text())
    execute = next(
        node for node in ast.walk(tree) if isinstance(node, ast.AsyncFunctionDef) and node.name == "execute"
    )
    guarded = [
        node
        for node in ast.walk(execute)
        if isinstance(node, ast.If)
        and "is None" in ast.unparse(node.test)
        and any("enqueue_event(task)" in ast.unparse(child) for child in node.body)
    ]
    assert guarded, f"{relative_source} enqueues the task without checking that it is new"
