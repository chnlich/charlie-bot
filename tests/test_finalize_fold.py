"""The finalize-judgment fold: O(1) answers pinned equal to the pure scans.

The fold in ``src/core/chat_events.py`` derives the two finalize idempotency
judgments (``src/core/finalize_effects``) incrementally from the cached event
list. Every test here pins fold answers against the pure functions over the
same list — the two implementations may not drift.
"""

from __future__ import annotations

import random
from pathlib import Path

import pytest
from conftest import build_worktree_cfg

from src.core import event_types as ET
from src.core import finalize_effects
from src.core.config import CharlieBotConfig
from src.core.models import CreateSessionRequest
from src.core.sessions import SessionManager

_MASTER_OUTPUT_TYPES = (ET.ASSISTANT, ET.MASTER_DONE, ET.ASSISTANT_ERROR)


def _summary(thread_id: str, status: str = "completed") -> dict:
  return {"type": ET.WORKER_SUMMARY, "thread_id": thread_id, "status": status}


def _assert_parity(mgr: SessionManager, sid: str, threads: tuple[str, str]) -> None:
  events = mgr.load_chat_events_sync(sid)
  for thread_id in threads:
    assert mgr._chat_events.finalize_summary_present(sid, thread_id) == \
        finalize_effects.terminal_summary_present(events, thread_id)
    assert mgr._chat_events.finalize_master_woke(sid, thread_id) == \
        finalize_effects.master_woke_after_summary(events, thread_id)


@pytest.mark.asyncio
async def test_fold_matches_pure_scans_over_randomized_appends(tmp_path: Path) -> None:
  cfg: CharlieBotConfig = build_worktree_cfg(tmp_path)
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name="fold-parity"))
  sid = session.id
  rng = random.Random(20260907)
  threads = ("t1", "t2")
  kinds = [(*_MASTER_OUTPUT_TYPES, ET.WORKER_SUMMARY, ET.USER, "error")]
  # 120 appends: enough kinds and interleavings to pin fold-vs-scan parity
  # without pricing the parity property by corpus volume.
  for i in range(120):
    kind = rng.choice(kinds[0])
    if kind == ET.WORKER_SUMMARY:
      event = _summary(rng.choice(threads), status=rng.choice(["completed", "failed", "running"]))
    else:
      event = {"type": kind, "content": f"chunk {i}"}
    await mgr.save_chat_event(sid, event)
    _assert_parity(mgr, sid, threads)


@pytest.mark.asyncio
async def test_fold_rebuilds_after_clear_and_reload(tmp_path: Path) -> None:
  cfg: CharlieBotConfig = build_worktree_cfg(tmp_path)
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name="fold-rebuild"))
  sid = session.id
  await mgr.save_chat_event(sid, _summary("t1"))
  await mgr.save_chat_event(sid, {"type": ET.MASTER_DONE})
  assert await mgr.finalize_summary_present(sid, "t1") is True

  mgr._chat_events.clear_cache(sid)
  assert await mgr.finalize_summary_present(sid, "t1") is True
  assert await mgr.finalize_master_woke(sid, "t1") is True
  _assert_parity(mgr, sid, ("t1",))
