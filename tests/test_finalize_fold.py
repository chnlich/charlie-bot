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
from src.core.chat_events import _FinalizeFold
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
  kinds = [_MASTER_OUTPUT_TYPES + (ET.WORKER_SUMMARY, ET.USER, "error")]
  for i in range(400):
    kind = rng.choice(kinds[0])
    if kind == ET.WORKER_SUMMARY:
      event = _summary(rng.choice(threads), status=rng.choice(["completed", "failed", "running"]))
    else:
      event = {"type": kind, "content": f"chunk {i}"}
    await mgr.save_chat_event(sid, event)
    _assert_parity(mgr, sid, threads)


@pytest.mark.asyncio
async def test_fold_answers_and_woke_reset_on_re_summary(tmp_path: Path) -> None:
  cfg: CharlieBotConfig = build_worktree_cfg(tmp_path)
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name="fold-reset"))
  sid = session.id

  await mgr.save_chat_event(sid, {"type": ET.USER, "content": "go"})
  assert await mgr.finalize_summary_present(sid, "t1") is False
  assert await mgr.finalize_master_woke(sid, "t1") is False

  await mgr.save_chat_event(sid, _summary("t1"))
  assert await mgr.finalize_summary_present(sid, "t1") is True
  assert await mgr.finalize_master_woke(sid, "t1") is False

  await mgr.save_chat_event(sid, {"type": ET.ASSISTANT, "content": "woke"})
  assert await mgr.finalize_master_woke(sid, "t1") is True
  assert await mgr.finalize_master_woke(sid, "t2") is False

  await mgr.save_chat_event(sid, _summary("t1"))
  assert await mgr.finalize_master_woke(sid, "t1") is False

  await mgr.save_chat_event(sid, _summary("t1", status="running"))
  assert await mgr.finalize_summary_present(sid, "t1") is True


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


@pytest.mark.asyncio
async def test_judgment_methods_load_cold_cache_off_the_list(tmp_path: Path) -> None:
  cfg: CharlieBotConfig = build_worktree_cfg(tmp_path)
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name="fold-cold"))
  sid = session.id
  await mgr.save_chat_event(sid, _summary("t1"))

  # Simulate the cold cache a server start leaves behind: the file exists, the
  # cache does not. The judgment must answer from a threaded load, and the
  # second call answers from the warm fold.
  mgr._chat_events.clear_cache(sid)
  assert mgr._chat_events.peek_cached_events(sid) is None
  assert await mgr.finalize_summary_present(sid, "t1") is True
  assert mgr._chat_events.peek_cached_events(sid) is not None
  assert await mgr.finalize_master_woke(sid, "t1") is False


def test_fold_unit_answers_match_pure_functions() -> None:
  fold = _FinalizeFold()
  events: list[dict] = []
  rng = random.Random(7)
  threads = ("t1", "t2")
  for _ in range(300):
    kind = rng.choice(_MASTER_OUTPUT_TYPES + (ET.WORKER_SUMMARY, ET.USER))
    if kind == ET.WORKER_SUMMARY:
      event = _summary(rng.choice(threads), status=rng.choice(["completed", "running"]))
    else:
      event = {"type": kind}
    events.append(event)
    fold.absorb(event)
    for thread_id in threads:
      assert fold.summary_present(thread_id) == finalize_effects.terminal_summary_present(events, thread_id)
      assert fold.master_woke(thread_id) == finalize_effects.master_woke_after_summary(events, thread_id)
