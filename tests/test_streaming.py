"""Tests for src/core/streaming.py (StreamingManager)."""

import asyncio
import pytest
from unittest.mock import AsyncMock, MagicMock

from src.core.streaming import StreamingManager


def _make_ws():
    """Return a mock WebSocket that records sent JSON."""
    ws = MagicMock()
    ws.send_json = AsyncMock()
    ws.send_text = AsyncMock()
    return ws


@pytest.mark.asyncio
class TestStreamingManager:
    async def test_subscribe_and_subscriber_count(self):
        mgr = StreamingManager()
        ws = _make_ws()
        assert mgr.subscriber_count("thread-1") == 0
        await mgr.subscribe("thread-1", ws)
        assert mgr.subscriber_count("thread-1") == 1

    async def test_unsubscribe_removes_connection(self):
        mgr = StreamingManager()
        ws = _make_ws()
        await mgr.subscribe("t1", ws)
        await mgr.unsubscribe("t1", ws)
        assert mgr.subscriber_count("t1") == 0

    async def test_unsubscribe_removes_key_when_empty(self):
        mgr = StreamingManager()
        ws = _make_ws()
        await mgr.subscribe("t1", ws)
        await mgr.unsubscribe("t1", ws)
        assert "t1" not in mgr._connections

    async def test_broadcast_sends_to_all_subscribers(self):
        mgr = StreamingManager()
        ws1 = _make_ws()
        ws2 = _make_ws()
        await mgr.subscribe("t1", ws1)
        await mgr.subscribe("t1", ws2)
        event = {"type": "output", "content": "hello"}
        await mgr.broadcast("t1", event)
        ws1.send_json.assert_awaited_once_with(event)
        ws2.send_json.assert_awaited_once_with(event)

    async def test_broadcast_no_subscribers_is_noop(self):
        mgr = StreamingManager()
        # Should not raise
        await mgr.broadcast("no-such-thread", {"type": "ping"})

    async def test_broadcast_removes_dead_connections(self):
        mgr = StreamingManager()
        ws_dead = _make_ws()
        ws_dead.send_json = AsyncMock(side_effect=Exception("disconnected"))
        ws_ok = _make_ws()

        await mgr.subscribe("t1", ws_dead)
        await mgr.subscribe("t1", ws_ok)

        await mgr.broadcast("t1", {"type": "ping"})

        # Dead WS should be pruned, live one remains
        assert ws_dead not in mgr._connections.get("t1", set())
        assert ws_ok in mgr._connections.get("t1", set())

    async def test_multiple_threads_isolated(self):
        mgr = StreamingManager()
        ws_a = _make_ws()
        ws_b = _make_ws()
        await mgr.subscribe("thread-A", ws_a)
        await mgr.subscribe("thread-B", ws_b)

        await mgr.broadcast("thread-A", {"type": "event-A"})

        ws_a.send_json.assert_awaited_once()
        ws_b.send_json.assert_not_awaited()

    async def test_unsubscribe_non_subscribed_is_safe(self):
        mgr = StreamingManager()
        ws = _make_ws()
        # Should not raise even if ws was never subscribed
        await mgr.unsubscribe("t1", ws)

    async def test_subscriber_count_zero_for_unknown_thread(self):
        mgr = StreamingManager()
        assert mgr.subscriber_count("unknown") == 0
