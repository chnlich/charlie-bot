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
import pathlib
from collections.abc import Iterator
from unittest import mock

import conftest
import pytest

from src.infra import config, models
from src.infra import event_types as ET
from src.runtime import sessions as sessions_module
from src.runtime.session_store import SessionStore


async def _seed_session(mgr: sessions_module.SessionManager) -> str:
  session = await conftest.create_root_session(mgr, models.CreateSessionRequest(name="catchup"))
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
          **conftest.assistant_text_event("seed answer"), "timestamp": "2026-09-07T00:00:01Z"
      })
  return session.id


@pytest.mark.asyncio
async def test_catchup_restores_stream_deltas_and_live_feed_broadcasts(tmp_path: pathlib.Path) -> None:
  cfg = config.CharlieBotConfig(charliebot_home=tmp_path / "home", backends=conftest.fake_backends())
  mgr = sessions_module.SessionManager(cfg, SessionStore(cfg))
  sid = await _seed_session(mgr)

  aggregator = await mgr._get_or_init_aggregator(sid)
  assert aggregator.emit_stream_deltas is True
  assert mgr._aggregators[sid] is aggregator

  with mock.patch(conftest.BROADCAST_PATCH_TARGET, new=mock.AsyncMock()) as broadcast:
    await mgr.persist_and_broadcast(
        sid, {
            **conftest.assistant_text_event("live tail"), "timestamp": "2026-09-07T00:00:02Z"
        })

  delta_types = [call.args[1]["type"] for call in broadcast.await_args_list]
  assert "stream" in delta_types


@pytest.mark.asyncio
async def test_concurrent_first_persists_catch_up_once(tmp_path: pathlib.Path) -> None:
  cfg = config.CharlieBotConfig(charliebot_home=tmp_path / "home", backends=conftest.fake_backends())
  mgr = sessions_module.SessionManager(cfg, SessionStore(cfg))
  sid = await _seed_session(mgr)

  inits = 0
  original = sessions_module.SessionManager._init_live_aggregator

  async def counting_init(self, session_id: str, epoch: int) -> sessions_module.MessageAggregator | None:
    nonlocal inits
    inits += 1
    return await original(self, session_id, epoch)

  with (
      mock.patch(conftest.BROADCAST_PATCH_TARGET, new=mock.AsyncMock()),
      mock.patch.object(sessions_module.SessionManager, "_init_live_aggregator", counting_init),
  ):
    first, second = await asyncio.gather(
        mgr._get_or_init_aggregator(sid),
        mgr._get_or_init_aggregator(sid),
    )

  assert inits == 1
  assert first is second


@pytest.mark.asyncio
async def test_drop_mid_feed_discards_and_reruns(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = config.CharlieBotConfig(charliebot_home=tmp_path / "home", backends=conftest.fake_backends())
  mgr = sessions_module.SessionManager(cfg, SessionStore(cfg))
  sid = await _seed_session(mgr)

  real_aggregator = sessions_module.MessageAggregator
  feeds = 0

  class DropMidFeed(real_aggregator):

    def feed(self, event: dict) -> Iterator[dict]:
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
