"""Tests for the live aggregator's lazy disk catch-up behind persist_and_broadcast.

The first persist_and_broadcast for a session after server start catches the
live aggregator up to the whole on-disk corpus; the catch-up runs in a thread,
emits no stream deltas (they are built and discarded otherwise), and restores
stream-delta emission before the aggregator carries the live feed.
"""

from __future__ import annotations

import asyncio
from unittest.mock import AsyncMock, patch

import pytest

from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.models import CreateSessionRequest
from src.core.sessions import SessionManager
from conftest import BROADCAST_PATCH_TARGET


async def _seed_session(mgr: SessionManager) -> str:
  session = await mgr.create_session(CreateSessionRequest(name="catchup"))
  await mgr.save_chat_event(session.id, {
      "type": ET.USER,
      "message": {"content": [{"type": "text", "text": "seed question"}]},
      "timestamp": "2026-09-07T00:00:00Z",
  })
  await mgr.save_chat_event(session.id, {
      "type": ET.ASSISTANT,
      "message": {"content": [{"type": "text", "text": "seed answer"}]},
      "timestamp": "2026-09-07T00:00:01Z",
  })
  return session.id


@pytest.mark.asyncio
async def test_catchup_restores_stream_deltas_and_live_feed_broadcasts(tmp_path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  sid = await _seed_session(mgr)

  aggregator = await mgr._get_or_init_aggregator(sid)
  assert aggregator.emit_stream_deltas is True
  assert mgr._aggregators[sid] is aggregator

  with patch(BROADCAST_PATCH_TARGET, new=AsyncMock()) as broadcast:
    await mgr.persist_and_broadcast(sid, {
        "type": ET.ASSISTANT,
        "message": {"content": [{"type": "text", "text": "live tail"}]},
        "timestamp": "2026-09-07T00:00:02Z",
    })

  delta_types = [call.args[1]["type"] for call in broadcast.await_args_list]
  assert "stream" in delta_types


@pytest.mark.asyncio
async def test_concurrent_first_persists_catch_up_once(tmp_path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  sid = await _seed_session(mgr)

  inits = 0
  original = SessionManager._init_live_aggregator

  def counting_init(self, session_id):
    nonlocal inits
    inits += 1
    return original(self, session_id)

  with patch(BROADCAST_PATCH_TARGET, new=AsyncMock()):
    with patch.object(SessionManager, "_init_live_aggregator", counting_init):
      first, second = await asyncio.gather(
          mgr._get_or_init_aggregator(sid),
          mgr._get_or_init_aggregator(sid),
      )

  assert inits == 1
  assert first is second


@pytest.mark.asyncio
async def test_drop_during_catchup_discards_stale_init(tmp_path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  sid = await _seed_session(mgr)

  original = SessionManager._init_live_aggregator
  calls = 0

  def dropping_init(self, session_id):
    nonlocal calls
    calls += 1
    aggregator = original(self, session_id)
    if calls == 1:
      # A drop landing while the threaded catch-up runs must win over it.
      self._drop_session_runtime_state(session_id)
    return aggregator

  with patch.object(SessionManager, "_init_live_aggregator", dropping_init):
    aggregator = await mgr._get_or_init_aggregator(sid)

  assert calls == 2
  assert mgr._aggregators[sid] is aggregator
  assert aggregator.emit_stream_deltas is True
