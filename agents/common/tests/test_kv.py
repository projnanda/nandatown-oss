"""NasikoKV: the key-value facade, including cursor-following listings."""

from __future__ import annotations

import pytest
from fake_state_server import MAX_PAGE_SIZE
from nasiko_state import NasikoKV

pytestmark = pytest.mark.anyio


@pytest.fixture(params=["sync", "async"])
def style(request: pytest.FixtureRequest) -> str:
    return request.param


@pytest.fixture
def kv(client) -> NasikoKV:
    return NasikoKV(client)


async def call(kv: NasikoKV, style: str, method: str, *args, **kwargs):
    if style == "sync":
        return getattr(kv, method)(*args, **kwargs)
    return await getattr(kv, "a" + method)(*args, **kwargs)


async def keys_under(kv: NasikoKV, style: str, prefix: str = "") -> list[str]:
    if style == "sync":
        return [record.key for record in kv.list(prefix)]
    return [record.key async for record in kv.alist(prefix)]


async def test_put_get_delete(kv, style):
    receipt = await call(kv, style, "put", "greeting", {"hello": ["world"]})
    record = await call(kv, style, "get", "greeting")

    assert receipt.version == 1
    assert (record.key, record.value) == ("greeting", {"hello": ["world"]})
    assert await call(kv, style, "delete", "greeting") is True
    assert await call(kv, style, "get", "greeting") is None
    assert await call(kv, style, "delete", "greeting") is False


async def test_listing_follows_cursors_across_pages(kv, style):
    total = MAX_PAGE_SIZE * 2 + 37
    for n in range(total):
        await call(kv, style, "put", f"item:{n:04d}", n)
    await call(kv, style, "put", "other", 0)

    keys = await keys_under(kv, style, "item:")

    assert keys == [f"item:{n:04d}" for n in range(total)]


async def test_listing_an_empty_prefix_returns_everything(kv, style):
    for key in ("a", "b"):
        await call(kv, style, "put", key, 1)

    assert await keys_under(kv, style) == ["a", "b"]


async def test_listing_a_prefix_nothing_matches_is_empty(kv, style):
    await call(kv, style, "put", "a", 1)

    assert await keys_under(kv, style, "zzz") == []


async def test_if_match_is_passed_through(kv, style):
    from nasiko_state import NasikoStateConflict

    await call(kv, style, "put", "k", 1)
    await call(kv, style, "put", "k", 2)

    with pytest.raises(NasikoStateConflict):
        await call(kv, style, "put", "k", 3, if_match=1)
    assert (await call(kv, style, "put", "k", 3, if_match=2)).version == 3


async def test_kv_writes_register_for_kv_only(kv, style, state_server):
    await call(kv, style, "put", "k", 1)

    assert [r["capabilities"] for r in state_server.registrations] == [["kv"]]
