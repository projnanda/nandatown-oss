"""Rows written by the JavaScript store (js/nasiko-state) are readable here, and the reverse."""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from sdk_compat import A2A_V1

JS_FIXTURES = Path(__file__).resolve().parents[1] / "js" / "nasiko-state" / "test" / "fixtures"

pytestmark = pytest.mark.skipif(not A2A_V1, reason="the JavaScript store writes the a2a 1.x format")


def fixture(name: str) -> dict:
    return json.loads((JS_FIXTURES / name).read_text())


def test_a_row_written_by_the_javascript_store_decodes_with_every_field():
    from nasiko_state import _decode_stored_task, _state_name

    task = _decode_stored_task(fixture("js-task-a2a1.json"), "task-js")

    assert (task.context_id, _state_name(task)) == ("ctx-js", "input-required")
    assert task.status.message.parts[0].text == "Which city?"
    assert task.history[0].parts[0].text == "weather"
    assert (task.artifacts[0].name, task.artifacts[0].parts[0].text) == ("report", "draft")
    assert dict(task.metadata) == {"trace": "abc", "n": 3}


def test_the_python_store_encodes_the_task_the_javascript_fixture_holds_identically():
    from nasiko_state import _decode_stored_task, _stored_body

    body = fixture("js-task-a2a1.json")

    assert _stored_body(_decode_stored_task(body, "task-js")) == body
