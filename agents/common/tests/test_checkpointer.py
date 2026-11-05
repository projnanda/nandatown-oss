"""NasikoCheckpointSaver: langgraph's checkpointer contract over the platform's key-value state."""

from __future__ import annotations

import asyncio
import json
from typing import TypedDict

import nasiko_state
import pytest
from langchain_core.messages import AIMessage, HumanMessage
from langgraph.checkpoint.memory import MemorySaver
from langgraph.checkpoint.serde.jsonplus import JsonPlusSerializer
from langgraph.checkpoint.serde.types import ERROR, INTERRUPT, RESUME
from langgraph.graph import END, START, MessagesState, StateGraph
from langgraph.types import Command, interrupt
from nasiko_state import (
    NasikoCheckpointSaver,
    NasikoKV,
    NasikoStateError,
    NasikoStateMalformedRow,
    NasikoStateTooLarge,
)

pytestmark = pytest.mark.anyio


@pytest.fixture(params=["sync", "async"])
def style(request: pytest.FixtureRequest) -> str:
    return request.param


@pytest.fixture
def saver(client) -> NasikoCheckpointSaver:
    return NasikoCheckpointSaver(NasikoKV(client))


async def call(saver, style: str, method: str, *args, **kwargs):
    if style == "sync":
        return getattr(saver, method)(*args, **kwargs)
    return await getattr(saver, "a" + method)(*args, **kwargs)


async def listing(saver, style: str, config, **kwargs) -> list:
    if style == "sync":
        return list(saver.list(config, **kwargs))
    return [item async for item in saver.alist(config, **kwargs)]


def config_for(thread: str = "thread-1", namespace: str = "", checkpoint_id: str | None = None) -> dict:
    configurable = {"thread_id": thread, "checkpoint_ns": namespace}
    if checkpoint_id:
        configurable["checkpoint_id"] = checkpoint_id
    return {"configurable": configurable}


def checkpoint(checkpoint_id: str, *, version: int = 1, **values) -> dict:
    return {
        "v": 4,
        "id": checkpoint_id,
        "ts": "2026-01-01T00:00:00+00:00",
        "channel_values": values,
        "channel_versions": {name: version for name in values},
        "versions_seen": {"node": {name: version for name in values}},
        "updated_channels": list(values),
    }


async def put(saver, style, config, checkpoint_id: str, *, step: int = 0, source: str = "loop", **values):
    metadata = {"source": source, "step": step, "parents": {}}
    new_versions = {name: 1 for name in values}  # what langgraph passes: the channels this step changed
    return await call(
        saver, style, "put", config, checkpoint(checkpoint_id, **values), metadata, new_versions
    )


class TestCheckpoints:
    async def test_a_put_checkpoint_reads_back_whole(self, saver, style):
        saved = await put(saver, style, config_for(), "cp-1", step=3, messages=["hi", {"n": [1, 2]}], count=7)

        loaded = await call(saver, style, "get_tuple", saved)

        assert loaded.checkpoint == checkpoint("cp-1", messages=["hi", {"n": [1, 2]}], count=7)
        assert loaded.metadata == {"source": "loop", "step": 3, "parents": {}}
        assert loaded.config == saved
        assert loaded.parent_config is None
        assert loaded.pending_writes == []

    async def test_put_returns_the_config_of_what_it_stored(self, saver, style):
        saved = await put(saver, style, config_for("t", "ns"), "cp-1")

        assert saved == {"configurable": {"thread_id": "t", "checkpoint_ns": "ns", "checkpoint_id": "cp-1"}}

    async def test_without_a_checkpoint_id_the_newest_is_returned(self, saver, style):
        first = await put(saver, style, config_for(), "cp-1", n=1)
        await put(saver, style, first, "cp-2", n=2)

        newest = await call(saver, style, "get_tuple", config_for())

        assert newest.checkpoint["id"] == "cp-2"
        assert newest.checkpoint["channel_values"] == {"n": 2}
        assert newest.config == config_for(checkpoint_id="cp-2")

    async def test_an_explicit_checkpoint_id_reads_that_one_and_returns_the_callers_config(
        self, saver, style
    ):
        first = await put(saver, style, config_for(), "cp-1", n=1)
        await put(saver, style, first, "cp-2", n=2)
        asked = config_for(checkpoint_id="cp-1")

        older = await call(saver, style, "get_tuple", asked)

        assert older.checkpoint["channel_values"] == {"n": 1}
        assert older.config is asked

    async def test_parent_config_points_at_the_checkpoint_it_was_made_from(self, saver, style):
        first = await put(saver, style, config_for(), "cp-1")
        await put(saver, style, first, "cp-2")

        child = await call(saver, style, "get_tuple", config_for(checkpoint_id="cp-2"))

        assert child.parent_config == config_for(checkpoint_id="cp-1")

    async def test_unknown_threads_and_checkpoints_read_as_none(self, saver, style):
        await put(saver, style, config_for(), "cp-1")

        assert await call(saver, style, "get_tuple", config_for("nobody")) is None
        assert await call(saver, style, "get_tuple", config_for(checkpoint_id="cp-9")) is None
        assert await call(saver, style, "get_tuple", config_for(namespace="other")) is None

    async def test_a_head_whose_checkpoint_has_expired_reads_as_none(self, saver, style, state_server):
        await put(saver, style, config_for(), "cp-1")
        del state_server.kv["lgh:thread-1:"].value["value"]["checkpoint"]  # a head naming it only
        del state_server.kv["lg:thread-1::cp-1"]  # the platform expired the older row

        assert await call(saver, style, "get_tuple", config_for()) is None

    async def test_namespaces_are_separate_timelines(self, saver, style):
        await put(saver, style, config_for(namespace=""), "cp-1", where="root")
        await put(saver, style, config_for(namespace="child:abc|grandchild:def"), "cp-2", where="child")

        root = await call(saver, style, "get_tuple", config_for(namespace=""))
        child = await call(saver, style, "get_tuple", config_for(namespace="child:abc|grandchild:def"))

        assert root.checkpoint["channel_values"] == {"where": "root"}
        assert child.checkpoint["channel_values"] == {"where": "child"}

    async def test_a_missing_namespace_means_the_root_namespace(self, saver, style):
        config = {"configurable": {"thread_id": "t"}}
        saved = await put(saver, style, config, "cp-1")

        assert saved["configurable"]["checkpoint_ns"] == ""
        assert (await call(saver, style, "get_tuple", config)).checkpoint["id"] == "cp-1"

    async def test_metadata_from_the_config_is_merged_like_the_in_memory_saver_does(self, saver, style):
        config = {
            "configurable": {"thread_id": "t", "checkpoint_ns": "", "user": "ada"},
            "metadata": {"run": "r1"},
        }

        saved = await put(saver, style, config, "cp-1")
        loaded = await call(saver, style, "get_tuple", saved)

        expected = MemorySaver()
        memory_saved = expected.put(
            config, checkpoint("cp-1"), {"source": "loop", "step": 0, "parents": {}}, {}
        )
        assert loaded.metadata == expected.get_tuple(memory_saved).metadata


class TestPendingWrites:
    async def test_writes_come_back_with_the_checkpoint_ordered_by_task_and_index(self, saver, style):
        saved = await put(saver, style, config_for(), "cp-1")
        await call(saver, style, "put_writes", saved, [("a", 1), ("b", {"x": 2})], "task-2")
        await call(saver, style, "put_writes", saved, [("c", [3])], "task-1")

        loaded = await call(saver, style, "get_tuple", saved)

        assert loaded.pending_writes == [("task-1", "c", [3]), ("task-2", "a", 1), ("task-2", "b", {"x": 2})]

    async def test_special_channels_use_their_own_negative_indexes(self, saver, style):
        saved = await put(saver, style, config_for(), "cp-1")
        await call(
            saver, style, "put_writes", saved, [("a", 1), (INTERRUPT, "pause"), (ERROR, "boom")], "task-1"
        )

        loaded = await call(saver, style, "get_tuple", saved)

        # ERROR is -1 and INTERRUPT is -3, so sorted by index the interrupt comes first.
        assert loaded.pending_writes == [
            ("task-1", INTERRUPT, "pause"),
            ("task-1", ERROR, "boom"),
            ("task-1", "a", 1),
        ]

    async def test_all_regular_writes_of_a_call_land_in_one_record(self, saver, style, state_server):
        saved = await put(saver, style, config_for(), "cp-1")
        state_server.requests.clear()

        await call(saver, style, "put_writes", saved, [("a", 1), ("b", 2), ("c", 3)], "task-1")

        assert len(state_server.writes("kv")) == 1  # one PUT, so a crash cannot split the task's writes

    async def test_regular_writes_saved_again_replace_the_earlier_ones(self, saver, style):
        saved = await put(saver, style, config_for(), "cp-1")
        await call(saver, style, "put_writes", saved, [("a", "first")], "task-1")
        await call(saver, style, "put_writes", saved, [("a", "second")], "task-1")

        loaded = await call(saver, style, "get_tuple", saved)

        assert loaded.pending_writes == [("task-1", "a", "second")]

    async def test_special_channel_writes_are_upserts(self, saver, style):
        saved = await put(saver, style, config_for(), "cp-1")
        await call(saver, style, "put_writes", saved, [(RESUME, "old")], "task-1")
        await call(saver, style, "put_writes", saved, [(RESUME, "new")], "task-1")

        loaded = await call(saver, style, "get_tuple", saved)

        assert loaded.pending_writes == [("task-1", RESUME, "new")]

    async def test_a_repeated_special_channel_in_one_call_keeps_the_last(self, saver, style):
        saved = await put(saver, style, config_for(), "cp-1")

        await call(saver, style, "put_writes", saved, [(INTERRUPT, "one"), (INTERRUPT, "two")], "task-1")

        assert (await call(saver, style, "get_tuple", saved)).pending_writes == [("task-1", INTERRUPT, "two")]

    async def test_writes_belong_to_their_own_checkpoint(self, saver, style):
        first = await put(saver, style, config_for(), "cp-1")
        second = await put(saver, style, first, "cp-2")
        await call(saver, style, "put_writes", first, [("a", 1)], "task-1")
        await call(saver, style, "put_writes", second, [("a", 2)], "task-1")

        assert (await call(saver, style, "get_tuple", first)).pending_writes == [("task-1", "a", 1)]
        assert (await call(saver, style, "get_tuple", second)).pending_writes == [("task-1", "a", 2)]

    async def test_writes_to_nothing_are_nothing(self, saver, style, state_server):
        saved = await put(saver, style, config_for(), "cp-1")
        state_server.requests.clear()

        await call(saver, style, "put_writes", saved, [], "task-1")

        assert state_server.requests == []

    async def test_task_path_is_accepted_and_defaults_to_empty(self, saver, style):
        saved = await put(saver, style, config_for(), "cp-1")

        await call(saver, style, "put_writes", saved, [("a", 1)], "task-1")
        await call(saver, style, "put_writes", saved, [("b", 2)], "task-2", "node|2")

        loaded = await call(saver, style, "get_tuple", saved)
        assert loaded.pending_writes == [("task-1", "a", 1), ("task-2", "b", 2)]


class TestListing:
    @pytest.fixture
    async def history(self, saver, style):
        config = config_for()
        for step in range(1, 6):
            config = await put(
                saver, style, config, f"cp-{step}", step=step, source="loop" if step > 1 else "input"
            )
        return config

    async def test_lists_newest_first(self, saver, style, history):
        items = await listing(saver, style, config_for())

        assert [item.checkpoint["id"] for item in items] == ["cp-5", "cp-4", "cp-3", "cp-2", "cp-1"]

    async def test_before_and_limit(self, saver, style, history):
        items = await listing(saver, style, config_for(), before=config_for(checkpoint_id="cp-4"), limit=2)

        assert [item.checkpoint["id"] for item in items] == ["cp-3", "cp-2"]

    async def test_a_limit_of_zero_returns_nothing(self, saver, style, history):
        assert await listing(saver, style, config_for(), limit=0) == []

    async def test_filters_on_metadata(self, saver, style, history):
        items = await listing(saver, style, config_for(), filter={"source": "input"})

        assert [item.checkpoint["id"] for item in items] == ["cp-1"]

    async def test_a_config_with_a_checkpoint_id_narrows_to_it(self, saver, style, history):
        items = await listing(saver, style, config_for(checkpoint_id="cp-2"))

        assert [item.checkpoint["id"] for item in items] == ["cp-2"]

    async def test_listed_checkpoints_carry_their_pending_writes_and_parents(self, saver, style, history):
        await call(saver, style, "put_writes", config_for(checkpoint_id="cp-3"), [("a", 1)], "task-1")

        items = {item.checkpoint["id"]: item for item in await listing(saver, style, config_for())}

        assert items["cp-3"].pending_writes == [("task-1", "a", 1)]
        assert items["cp-4"].pending_writes == []
        assert items["cp-4"].parent_config == config_for(checkpoint_id="cp-3")

    async def test_other_threads_and_namespaces_are_not_listed(self, saver, style, history):
        await put(saver, style, config_for("other"), "cp-x")
        await put(saver, style, config_for(namespace="child"), "cp-y")

        only_root = await listing(saver, style, config_for(namespace=""))
        thread_wide = await listing(saver, style, {"configurable": {"thread_id": "thread-1"}})

        assert [i.checkpoint["id"] for i in only_root] == ["cp-5", "cp-4", "cp-3", "cp-2", "cp-1"]
        assert "cp-y" in [i.checkpoint["id"] for i in thread_wide]
        assert "cp-x" not in [i.checkpoint["id"] for i in thread_wide]

    async def test_no_config_lists_every_thread(self, saver, style, history):
        await put(saver, style, config_for("other"), "cp-x")

        items = await listing(saver, style, None)

        assert {i.config["configurable"]["thread_id"] for i in items} == {"thread-1", "other"}

    async def test_listing_matches_the_in_memory_saver(self, saver, style):
        memory = MemorySaver()
        config = mem_config = config_for()
        for step in range(1, 5):
            # A changed channel gets a new version, as langgraph guarantees; the in-memory saver
            # shares one stored value per (channel, version).
            step_checkpoint = checkpoint(f"cp-{step}", version=step, n=step)
            metadata = {"source": "loop", "step": step, "parents": {}}
            config = await call(saver, style, "put", config, step_checkpoint, metadata, {"n": step})
            mem_config = memory.put(mem_config, step_checkpoint, metadata, {"n": step})
            await call(saver, style, "put_writes", config, [("n", step)], f"task-{step}")
            memory.put_writes(mem_config, [("n", step)], f"task-{step}")

        ours = await listing(saver, style, config_for(), before=config_for(checkpoint_id="cp-4"))
        theirs = list(memory.list(config_for(), before=config_for(checkpoint_id="cp-4")))

        assert [(i.checkpoint, i.metadata, i.parent_config, i.pending_writes) for i in ours] == [
            (i.checkpoint, i.metadata, i.parent_config, i.pending_writes) for i in theirs
        ]


class TestDeletion:
    async def test_delete_thread_removes_its_checkpoints_writes_and_head_only(
        self, saver, style, state_server
    ):
        for thread in ("doomed", "kept"):
            saved = await put(saver, style, config_for(thread), "cp-1")
            await call(saver, style, "put_writes", saved, [("a", 1)], "task-1")

        await call(saver, style, "delete_thread", "doomed")

        assert await call(saver, style, "get_tuple", config_for("doomed")) is None
        assert (await call(saver, style, "get_tuple", config_for("kept"))).pending_writes == [
            ("task-1", "a", 1)
        ]
        assert not [key for key in state_server.kv if key.split(":")[1] == "doomed"]

    async def test_deleting_a_thread_that_does_not_exist_is_fine(self, saver, style):
        await call(saver, style, "delete_thread", "nobody")

    async def test_a_thread_id_that_prefixes_another_does_not_delete_it(self, saver, style):
        await put(saver, style, config_for("abc"), "cp-1")
        await put(saver, style, config_for("abcd"), "cp-1")

        await call(saver, style, "delete_thread", "abc")

        assert await call(saver, style, "get_tuple", config_for("abcd")) is not None


class TestKeys:
    async def test_components_containing_the_separator_cannot_collide(self, saver, style):
        await put(saver, style, config_for("a:b", "c"), "cp-1", which="first")
        await put(saver, style, config_for("a", "b:c"), "cp-1", which="second")

        first = await call(saver, style, "get_tuple", config_for("a:b", "c"))
        second = await call(saver, style, "get_tuple", config_for("a", "b:c"))

        assert first.checkpoint["channel_values"] == {"which": "first"}
        assert second.checkpoint["channel_values"] == {"which": "second"}

    async def test_awkward_ids_round_trip_and_keys_stay_url_safe(self, saver, style, state_server):
        awkward = "thread/with:odd chars?and%percent|bar"
        saved = await put(saver, style, config_for(awkward, "ns|x:y"), "cp-1")
        await call(saver, style, "put_writes", saved, [("a", 1)], "task:1/2")

        loaded = await call(saver, style, "get_tuple", saved)

        assert loaded.pending_writes == [("task:1/2", "a", 1)]
        assert all(key.isascii() and "/" not in key and " " not in key for key in state_server.kv)

    async def test_the_documented_key_families(self, saver, style, state_server):
        saved = await put(saver, style, config_for("t"), "cp-1")
        await call(saver, style, "put_writes", saved, [("a", 1), ("b", 2)], "task-9")
        await call(saver, style, "put_writes", saved, [(INTERRUPT, "x")], "task-9")

        assert sorted(state_server.kv) == [
            "lg:t::cp-1",
            "lgh:t:",
            "lgw:t::cp-1:task-9:-3",
            "lgw:t::cp-1:task-9:0",
        ]


class TestCallCounts:
    """The platform rate-limits per agent, so what a checkpointer costs in calls is part of its contract."""

    async def test_put_is_two_writes_and_nothing_else(self, saver, style, state_server):
        await put(saver, style, config_for(), "cp-1")

        assert [(r.method, r.path.split("/")[1]) for r in state_server.requests if r.path != "/register"] == [
            ("PUT", "kv"),
            ("PUT", "kv"),
        ]

    async def test_get_tuple_of_the_newest_is_the_head_and_its_writes(self, saver, style, state_server):
        await put(saver, style, config_for(), "cp-1", messages=["hi"], notes={"n": 1})
        state_server.requests.clear()

        loaded = await call(saver, style, "get_tuple", config_for())

        assert [(r.method, r.path) for r in state_server.requests] == [
            ("GET", "/kv/lgh%3Athread-1%3A"),
            ("GET", "/kv"),  # the writes listing
        ]
        assert loaded.checkpoint["channel_values"] == {"messages": ["hi"], "notes": {"n": 1}}

    async def test_get_tuple_of_an_explicit_checkpoint_is_its_record_and_its_writes(
        self, saver, style, state_server
    ):
        saved = await put(saver, style, config_for(), "cp-1", messages=["hi"])
        state_server.requests.clear()

        await call(saver, style, "get_tuple", saved)

        assert [(r.method, r.path) for r in state_server.requests] == [
            ("GET", "/kv/lg%3Athread-1%3A%3Acp-1"),
            ("GET", "/kv"),
        ]

    async def test_put_writes_of_regular_writes_is_one_write_and_no_read(self, saver, style, state_server):
        saved = await put(saver, style, config_for(), "cp-1")
        state_server.requests.clear()

        await call(saver, style, "put_writes", saved, [("a", 1), ("b", 2)], "task-1")

        assert [r.method for r in state_server.requests] == ["PUT"]

    async def test_an_agent_turn_with_one_tool_call_is_three_calls_then_four(self, client, state_server):
        """The stock langgraph agent's turn: a ReAct graph run with durability="exit"."""
        saver = NasikoCheckpointSaver(NasikoKV(client))
        config = {"configurable": {"thread_id": "chat"}}
        calls_per_turn = []
        for turn in ("first", "second"):  # a new thread, then one with history to load
            graph = react_agent_with_one_tool_call(saver)
            state_server.requests.clear()
            async for _ in graph.astream(
                {"messages": [("user", turn)]}, config, stream_mode="values", durability="exit"
            ):
                pass
            calls_per_turn.append(len([r for r in state_server.requests if r.path != "/register"]))

        # new thread: the head (not found), the record, the head; then the history adds its writes listing
        assert calls_per_turn == [3, 4]


def versioned_checkpoint(checkpoint_id: str, **channels: tuple[int, object]) -> dict:
    """A checkpoint whose channels each carry their own version: name=(version, value)."""
    return {
        **checkpoint(checkpoint_id),
        "channel_values": {name: value for name, (_, value) in channels.items()},
        "channel_versions": {name: version for name, (version, _) in channels.items()},
    }


METADATA = {"source": "loop", "step": 0, "parents": {}}


@pytest.fixture
def spilling(monkeypatch):
    """Every checkpoint is too big for inline values, as a long thread's state eventually is."""
    monkeypatch.setattr(nasiko_state, "INLINE_CHECKPOINT_MAX_BYTES", 0)


class TestChannelValues:
    async def test_values_travel_inside_the_checkpoint_record(self, saver, style, state_server):
        await put(saver, style, config_for(), "cp-1", messages=["hi"], count=7, label="x")

        record = state_server.kv["lg:thread-1::cp-1"].value["value"]
        assert not [key for key in state_server.kv if key.startswith("lgb:")]
        assert set(record["values"]) == {"messages"}  # scalars are serialized with the checkpoint

    async def test_a_put_is_the_record_then_the_head(self, saver, style, state_server):
        first = versioned_checkpoint("cp-1", log=(1, ["a"]), notes=(1, {"big": "x" * 50}))
        saved = await call(saver, style, "put", config_for(), first, METADATA, {"log": 1, "notes": 1})
        state_server.requests.clear()

        second = versioned_checkpoint("cp-2", log=(2, ["a", "b"]), notes=(1, {"big": "x" * 50}))
        await call(saver, style, "put", saved, second, METADATA, {"log": 2})
        loaded = await call(saver, style, "get_tuple", config_for())

        assert [r.path for r in state_server.writes("kv")] == [
            "/kv/lg%3Athread-1%3A%3Acp-2",
            "/kv/lgh%3Athread-1%3A",
        ]
        assert loaded.checkpoint["channel_values"] == {"log": ["a", "b"], "notes": {"big": "x" * 50}}

    async def test_the_head_carries_the_newest_record(self, saver, style, state_server):
        await put(saver, style, config_for(), "cp-1", messages=["hi"])

        head = state_server.kv["lgh:thread-1:"].value["value"]

        assert head == {
            "checkpoint_id": "cp-1",
            "checkpoint": state_server.kv["lg:thread-1::cp-1"].value["value"],
        }

    async def test_a_big_state_spills_every_value_into_its_own_blob(
        self, saver, style, state_server, spilling
    ):
        first = versioned_checkpoint("cp-1", log=(1, ["a"]), notes=(1, {"big": "x" * 50}))
        saved = await call(saver, style, "put", config_for(), first, METADATA, {"log": 1, "notes": 1})
        state_server.requests.clear()

        second = versioned_checkpoint("cp-2", log=(2, ["a", "b"]), notes=(1, {"big": "x" * 50}))
        await call(saver, style, "put", saved, second, METADATA, {"log": 2})
        loaded = await call(saver, style, "get_tuple", config_for())

        assert [r.path for r in state_server.writes("kv")] == [
            "/kv/lgb%3Athread-1%3A%3Alog%3A2",
            "/kv/lgb%3Athread-1%3A%3Anotes%3A1",  # rewritten: it may have been inline before
            "/kv/lg%3Athread-1%3A%3Acp-2",
            "/kv/lgh%3Athread-1%3A",
        ]
        assert state_server.kv["lg:thread-1::cp-2"].value["value"]["values"] == {}
        assert loaded.checkpoint["channel_values"] == {"log": ["a", "b"], "notes": {"big": "x" * 50}}

    async def test_a_head_the_platform_refuses_only_names_the_checkpoint(self, saver, style, state_server):
        await put(saver, style, config_for(), "cp-1", messages=["hi"])
        record_size = len(json.dumps(state_server.kv["lg:thread-1::cp-1"].value["value"]))
        state_server.max_body_bytes = record_size  # the record fits, the head that carries it does not

        await put(saver, style, config_for(), "cp-1", messages=["hi"])
        loaded = await call(saver, style, "get_tuple", config_for())

        assert state_server.kv["lgh:thread-1:"].value["value"] == {"checkpoint_id": "cp-1"}
        assert loaded.checkpoint["channel_values"] == {"messages": ["hi"]}

    async def test_the_size_limit_applies_to_each_value_not_to_the_whole_state(
        self, saver, style, state_server
    ):
        state_server.max_body_bytes = 600
        halves = {"left": ["l" * 300], "right": ["r" * 300]}  # together over the limit, each under it

        saved = await put(saver, style, config_for(), "cp-1", **halves)

        # The inline record was refused, so it was written again with each value on its own.
        assert state_server.kv["lg:thread-1::cp-1"].value["value"]["blobs"] == {"left": 1, "right": 1}
        assert (await call(saver, style, "get_tuple", saved)).checkpoint["channel_values"] == halves

    async def test_a_value_over_the_limit_raises_a_typed_error(self, saver, style, state_server):
        state_server.max_body_bytes = 600

        with pytest.raises(NasikoStateTooLarge) as raised:
            await put(saver, style, config_for(), "cp-1", messages=["m" * 1000])

        assert raised.value.status == 413
        assert isinstance(raised.value, NasikoStateError)

    async def test_a_checkpoint_whose_blob_expired_is_an_error_not_a_missing_channel(
        self, saver, style, state_server, spilling
    ):
        await put(saver, style, config_for(), "cp-1", messages=["hi"])
        del state_server.kv["lgb:thread-1::messages:1"]

        with pytest.raises(NasikoStateMalformedRow, match="messages"):
            await call(saver, style, "get_tuple", config_for())

    async def test_listing_reads_a_shared_value_once(self, saver, style, state_server, spilling):
        config = config_for()
        for step in (1, 2, 3):
            snapshot = versioned_checkpoint(f"cp-{step}", log=(step, ["x"] * step), notes=(1, {"n": 1}))
            config = await call(saver, style, "put", config, snapshot, METADATA, {"log": step, "notes": 1})
        state_server.requests.clear()

        items = await listing(saver, style, config_for())

        blob_reads = [r.path for r in state_server.calls("GET", "/kv/lgb")]
        assert len(items) == 3
        assert len(blob_reads) == 4  # three versions of log, one notes value shared by all three
        assert all(item.checkpoint["channel_values"]["notes"] == {"n": 1} for item in items)

    async def test_delete_thread_removes_its_blobs(self, saver, style, state_server):
        await put(saver, style, config_for("doomed"), "cp-1", messages=["hi"])

        await call(saver, style, "delete_thread", "doomed")

        assert not state_server.kv


def react_agent_with_one_tool_call(checkpointer):
    """A prebuilt ReAct agent whose model calls one tool, then answers."""
    import warnings

    from langchain_core.language_models.fake_chat_models import GenericFakeChatModel
    from langchain_core.tools import tool

    class ToolCallingFake(GenericFakeChatModel):
        def bind_tools(self, tools, **kwargs):
            return self

    @tool
    def exchange_rate(currency: str) -> str:
        """The exchange rate of a currency."""
        return "1.1"

    replies = iter(
        [
            AIMessage(
                content="", tool_calls=[{"name": "exchange_rate", "args": {"currency": "EUR"}, "id": "c1"}]
            ),
            AIMessage(content="1 EUR is 1.1 USD"),
        ]
    )
    with warnings.catch_warnings():
        warnings.simplefilter("ignore")  # create_react_agent is deprecated in favour of langchain's
        from langgraph.prebuilt import create_react_agent

        return create_react_agent(
            ToolCallingFake(messages=replies), tools=[exchange_rate], checkpointer=checkpointer
        )


class TestMalformedRecords:
    """A damaged record is reported as such, typed, never as a bare KeyError from inside the saver."""

    async def test_a_head_without_a_checkpoint_id(self, saver, style, state_server):
        await put(saver, style, config_for(), "cp-1")
        state_server.kv["lgh:thread-1:"].value["value"] = {"unexpected": True}

        with pytest.raises(NasikoStateMalformedRow):
            await call(saver, style, "get_tuple", config_for())

    async def test_a_checkpoint_record_missing_fields(self, saver, style, state_server):
        saved = await put(saver, style, config_for(), "cp-1")
        del state_server.kv["lg:thread-1::cp-1"].value["value"]["metadata"]

        with pytest.raises(NasikoStateMalformedRow, match="metadata"):
            await call(saver, style, "get_tuple", saved)

    async def test_a_payload_that_is_not_base64(self, saver, style, state_server):
        saved = await put(saver, style, config_for(), "cp-1")
        state_server.kv["lg:thread-1::cp-1"].value["value"]["checkpoint"]["data"] = "not base64!"

        with pytest.raises(NasikoStateMalformedRow):
            await call(saver, style, "get_tuple", saved)

    async def test_a_head_carrying_another_checkpoint(self, saver, style, state_server):
        await put(saver, style, config_for(), "cp-1")
        state_server.kv["lgh:thread-1:"].value["value"]["checkpoint_id"] = "cp-9"

        with pytest.raises(NasikoStateMalformedRow, match="cp-9"):
            await call(saver, style, "get_tuple", config_for())

    async def test_a_damaged_record_carried_by_the_head(self, saver, style, state_server):
        await put(saver, style, config_for(), "cp-1")
        del state_server.kv["lgh:thread-1:"].value["value"]["checkpoint"]["metadata"]

        with pytest.raises(NasikoStateMalformedRow, match="metadata"):
            await call(saver, style, "get_tuple", config_for())

    async def test_a_writes_record_without_writes(self, saver, style, state_server):
        saved = await put(saver, style, config_for(), "cp-1")
        await call(saver, style, "put_writes", saved, [("a", 1)], "task-1")
        del state_server.kv["lgw:thread-1::cp-1:task-1:0"].value["value"]["writes"]

        with pytest.raises(NasikoStateMalformedRow):
            await call(saver, style, "get_tuple", saved)

    async def test_a_malformed_record_in_a_listing(self, saver, style, state_server):
        await put(saver, style, config_for(), "cp-1")
        state_server.kv["lg:thread-1::cp-1"].value["value"] = ["not", "a", "record"]

        with pytest.raises(NasikoStateMalformedRow):
            await listing(saver, style, config_for())


class TestSerialization:
    async def test_pickle_is_never_written(self, client, style):
        saver = NasikoCheckpointSaver(NasikoKV(client), serde=JsonPlusSerializer(pickle_fallback=True))

        with pytest.raises(NasikoStateError, match="pickle"):
            await put(saver, style, config_for(), "cp-1", unserializable=object())

    async def test_a_stored_pickle_is_refused_even_by_a_serde_that_allows_it(
        self, client, style, state_server
    ):
        saver = NasikoCheckpointSaver(NasikoKV(client), serde=JsonPlusSerializer(pickle_fallback=True))
        saved = await put(saver, style, config_for(), "cp-1")
        record = state_server.kv["lg:thread-1::cp-1"].value["value"]
        record["checkpoint"] = {"type": "pickle", "data": "gASVBAAAAAAAAABLAS4="}

        with pytest.raises(NasikoStateError, match="pickle"):
            await call(saver, style, "get_tuple", saved)

    async def test_records_are_plain_json_with_base64_payloads(self, saver, style, state_server):
        await put(saver, style, config_for(), "cp-1", n=1)

        record = state_server.kv["lg:thread-1::cp-1"].value["value"]

        assert set(record) == {
            "thread_id",
            "checkpoint_ns",
            "checkpoint_id",
            "parent_checkpoint_id",
            "checkpoint",
            "metadata",
            "values",
            "blobs",
        }
        assert set(record["checkpoint"]) == {"type", "data"}
        assert record["checkpoint"]["type"] == "msgpack"

    def test_it_registers_for_kv_only(self, saver, state_server):
        saver.put(config_for(), checkpoint("cp-1"), {"source": "loop", "step": 0, "parents": {}}, {})

        assert [r["capabilities"] for r in state_server.registrations] == [["kv"]]


class State(TypedDict, total=False):
    topic: str
    answer: str
    report: str


def ask_the_human(state: State) -> State:
    return {"answer": interrupt(f"Which angle on {state['topic']}?")}


def write_report(state: State) -> State:
    return {"report": f"{state['topic']}: {state['answer']}"}


def build_graph(checkpointer):
    builder = StateGraph(State)
    builder.add_node("ask", ask_the_human)
    builder.add_node("write", write_report)
    builder.add_edge(START, "ask")
    builder.add_edge("ask", "write")
    builder.add_edge("write", END)
    return builder.compile(checkpointer=checkpointer)


class TestPausedGraphsSurviveARestart:
    """The point of the checkpointer: work paused on interrupt() resumes after the process is gone."""

    def new_process(self, client) -> NasikoCheckpointSaver:
        return NasikoCheckpointSaver(NasikoKV(client))

    def test_sync(self, client):
        config = {"configurable": {"thread_id": "report-1"}}
        first = build_graph(self.new_process(client))
        paused = first.invoke({"topic": "tides"}, config)
        assert paused["__interrupt__"][0].value == "Which angle on tides?"
        del first  # the process dies: nothing but the platform's state is left

        second = build_graph(self.new_process(client))
        assert second.get_state(config).next == ("ask",)  # found in the store, not in memory
        finished = second.invoke(Command(resume="the moon"), config)

        assert finished["report"] == "tides: the moon"

    async def test_async(self, client):
        config = {"configurable": {"thread_id": "report-2"}}
        first = build_graph(self.new_process(client))
        paused = await first.ainvoke({"topic": "tides"}, config)
        assert paused["__interrupt__"][0].value == "Which angle on tides?"
        del first

        second = build_graph(self.new_process(client))
        assert (await second.aget_state(config)).next == ("ask",)
        finished = await second.ainvoke(Command(resume="the moon"), config)

        assert finished["report"] == "tides: the moon"

    def test_a_graph_with_no_durable_state_loses_the_pause(self):
        config = {"configurable": {"thread_id": "report-3"}}
        first = build_graph(MemorySaver())
        first.invoke({"topic": "tides"}, config)

        second = build_graph(MemorySaver())  # a restarted process with the in-memory saver

        assert second.get_state(config).next == ()

    def test_streaming_checkpoints_every_step(self, client, state_server):
        config = {"configurable": {"thread_id": "report-4"}}
        graph = build_graph(self.new_process(client))

        list(graph.stream({"topic": "tides"}, config, stream_mode="values"))

        assert len([key for key in state_server.kv if key.startswith("lg:report-4:")]) >= 2
        history = [
            snapshot.config["configurable"]["checkpoint_id"] for snapshot in graph.get_state_history(config)
        ]
        stored = [item.config["configurable"]["checkpoint_id"] for item in graph.checkpointer.list(config)]
        assert history == stored

    def test_resuming_with_two_threads_in_flight_keeps_them_apart(self, client):
        graph = build_graph(self.new_process(client))
        for thread, topic in (("a", "tides"), ("b", "winds")):
            graph.invoke({"topic": topic}, {"configurable": {"thread_id": thread}})

        restarted = build_graph(self.new_process(client))
        done_b = restarted.invoke(Command(resume="the sea"), {"configurable": {"thread_id": "b"}})
        done_a = restarted.invoke(Command(resume="the moon"), {"configurable": {"thread_id": "a"}})

        assert (done_a["report"], done_b["report"]) == ("tides: the moon", "winds: the sea")


def echo_graph(checkpointer):
    """A chat graph whose state is langchain messages, like the stock langgraph agent's."""

    def reply(state: MessagesState) -> MessagesState:
        heard = [m.content for m in state["messages"] if isinstance(m, HumanMessage)]
        return {"messages": [AIMessage(content=f"you said {heard[-1]!r} ({len(heard)} so far)")]}

    builder = StateGraph(MessagesState)
    builder.add_node("reply", reply)
    builder.add_edge(START, "reply")
    builder.add_edge("reply", END)
    return builder.compile(checkpointer=checkpointer)


class TestConversationsSurviveARestart:
    def test_langchain_messages_round_trip_through_the_store(self, client):
        config = {"configurable": {"thread_id": "chat-1"}}
        echo_graph(NasikoCheckpointSaver(NasikoKV(client))).invoke({"messages": [HumanMessage("hi")]}, config)

        restarted = echo_graph(NasikoCheckpointSaver(NasikoKV(client)))
        result = restarted.invoke({"messages": [HumanMessage("again")]}, config)

        assert [(type(m).__name__, m.content) for m in result["messages"]] == [
            ("HumanMessage", "hi"),
            ("AIMessage", "you said 'hi' (1 so far)"),
            ("HumanMessage", "again"),
            ("AIMessage", "you said 'again' (2 so far)"),
        ]

    async def test_async(self, client):
        config = {"configurable": {"thread_id": "chat-2"}}
        await echo_graph(NasikoCheckpointSaver(NasikoKV(client))).ainvoke(
            {"messages": [HumanMessage("hi")]}, config
        )

        restarted = echo_graph(NasikoCheckpointSaver(NasikoKV(client)))
        state = await restarted.aget_state(config)

        assert [m.content for m in state.values["messages"]] == ["hi", "you said 'hi' (1 so far)"]


class TestConstruction:
    def test_without_langgraph_the_saver_explains_itself(self, client, monkeypatch):
        import nasiko_state

        monkeypatch.setattr(nasiko_state, "_LANGGRAPH_AVAILABLE", False)

        with pytest.raises(ImportError, match="langgraph-checkpoint"):
            NasikoCheckpointSaver(NasikoKV(client))

    def test_with_allowlist_returns_a_working_clone(self, saver):
        clone = saver.with_allowlist([("langgraph", "types", "Send")])

        assert isinstance(clone, NasikoCheckpointSaver)
        assert clone._kv is saver._kv


async def test_concurrent_async_checkpoint_writes_do_not_interfere(client):
    saver = NasikoCheckpointSaver(NasikoKV(client))

    await asyncio.gather(
        *(
            saver.aput(
                config_for(f"thread-{n}"),
                checkpoint(f"cp-{n}", n=n),
                {"source": "loop", "step": n, "parents": {}},
                {},
            )
            for n in range(20)
        )
    )

    for n in range(20):
        assert (await saver.aget_tuple(config_for(f"thread-{n}"))).checkpoint["channel_values"] == {"n": n}
