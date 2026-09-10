"""Tests for the live aggregator's lazy disk catch-up behind persist_and_broadcast.

The first persist_and_broadcast for a session after server start catches the
live aggregator up to the whole on-disk corpus; the corpus load runs in a
thread and the feed runs in on-loop slices, emits no stream deltas (they are
built and discarded otherwise), and restores stream-delta emission before the
aggregator carries the live feed. A drop landing mid-init (its epoch bump
re-checked at every slice boundary) discards the unfinished init and reruns.
"""

from __future__ import annotations

import asyncio
from unittest.mock import AsyncMock, patch

import pytest
from conftest import BROADCAST_PATCH_TARGET

from src.core import event_types as ET
from src.core import sessions as sessions_module
from src.core.config import CharlieBotConfig
from src.core.models import CreateSessionRequest
from src.core.sessions import SessionManager


async def _seed_session(mgr: SessionManager) -> str:
  session = await mgr.create_session(CreateSessionRequest(name="catchup"))
  await mgr.save_chat_event(
      session.id, {
          "type": ET.USER,
          "message": {
              "content": [{
                  "type": "text",
                  "text": "seed question"
              }]
          },
          "timestamp": "2026-09-07T00:00:00Z",
      })
  await mgr.save_chat_event(
      session.id, {
          "type": ET.ASSISTANT,
          "message": {
              "content": [{
                  "type": "text",
                  "text": "seed answer"
              }]
          },
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
    await mgr.persist_and_broadcast(
        sid, {
            "type": ET.ASSISTANT,
            "message": {
                "content": [{
                    "type": "text",
                    "text": "live tail"
                }]
            },
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

  async def counting_init(self, session_id, epoch):
    nonlocal inits
    inits += 1
    return await original(self, session_id, epoch)

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

  original = SessionManager._load_aggregator_init_inputs
  loads = 0

  def dropping_load(self, session_id):
    nonlocal loads
    loads += 1
    inputs = original(self, session_id)
    if loads == 1:
      # A drop landing while the catch-up runs must win over it: the epoch
      # re-check at the first slice boundary discards this init.
      self._drop_session_runtime_state(session_id)
    return inputs

  with patch.object(SessionManager, "_load_aggregator_init_inputs", dropping_load):
    aggregator = await mgr._get_or_init_aggregator(sid)

  assert loads == 2
  assert mgr._aggregators[sid] is aggregator
  assert aggregator.emit_stream_deltas is True


@pytest.mark.asyncio
async def test_drop_mid_feed_discards_and_reruns(tmp_path, monkeypatch) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  sid = await _seed_session(mgr)

  real_aggregator = sessions_module.MessageAggregator
  feeds = 0

  class DropMidFeed(real_aggregator):

    def feed(self, event):
      nonlocal feeds
      feeds += 1
      if feeds == 2:
        # Lands inside the init's first slice; the slice-boundary epoch
        # re-check must discard the unfinished init.
        mgr._drop_session_runtime_state(sid)
      return super().feed(event)

  monkeypatch.setattr(sessions_module, "MessageAggregator", DropMidFeed)
  aggregator = await mgr._get_or_init_aggregator(sid)

  assert feeds >= 3  # the dropped run's feeds plus the rerun's
  assert mgr._aggregators[sid] is aggregator
  assert aggregator.emit_stream_deltas is True
