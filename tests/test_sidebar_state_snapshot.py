"""Tests for the dirty-session re-probe snapshot behind /api/sessions/status.

populate_sidebar_state serves clean sessions from the in-process snapshot in
src.core.sidebar_state (zero disk access), re-probes only dirty sessions, and
falls back to a full re-probe on every 10th call and via /status?force=1.
Every writer of the probed state must mark its session dirty at the
transition; disk stays the single source of truth.
"""

from __future__ import annotations

import asyncio
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest
from conftest import (
    append_events,
    make_home_session,
    write_plans,
    write_thread_meta,
    write_trigger,
)

from src.api import sessions as sessions_api
from src.core import sessions as sessions_core
from src.core import sidebar_state
from src.core.models import (
    CreateSessionRequest,
    PendingTrigger,
    SessionStatus,
    ThreadStatus,
)
from src.core.plans import PlanRegistryManager
from src.core.thinking_state import clear_busy, mark_busy
from src.core.threads import ThreadManager
from src.core.triggers import TriggerManager


@pytest.fixture(autouse=True)
def _clean_sidebar_state():
  """Isolate the module-level dirty set / snapshot / poll counter per test."""
  sidebar_state.reset_for_tests()
  yield
  sidebar_state.reset_for_tests()


def _counting_probes(monkeypatch: pytest.MonkeyPatch) -> dict[str, int]:
  """Route the pure probe cores through call counters (probe work visibility)."""
  calls = {"running": 0, "trigger": 0, "plan": 0}
  reals = {
      "running": sessions_core.has_running_tasks_sync,
      "trigger": sessions_core.pending_trigger_state_sync,
      "plan": sessions_core.has_pending_plan_approval_sync,
  }

  def _wrap(name: str):

    def wrapper(*args, **kwargs):
      calls[name] += 1
      return reals[name](*args, **kwargs)

    return wrapper

  monkeypatch.setattr(sessions_core, "has_running_tasks_sync", _wrap("running"))
  monkeypatch.setattr(sessions_core, "pending_trigger_state_sync", _wrap("trigger"))
  monkeypatch.setattr(sessions_core, "has_pending_plan_approval_sync", _wrap("plan"))
  return calls


# ---------------------------------------------------------------------------
# (a) dirty-mark propagation — one test per transition class
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_save_metadata_marks_session_dirty(tmp_path: Path) -> None:
  _cfg, mgr, session = await make_home_session(tmp_path, name="Meta")
  meta = await mgr.get_session(session.id)
  assert meta is not None
  sidebar_state.reset_for_tests()

  await mgr.save_metadata(meta)

  assert sidebar_state.is_dirty(session.id)


@pytest.mark.asyncio
async def test_thread_create_marks_session_dirty(tmp_path: Path) -> None:
  _cfg, mgr, session = await make_home_session(tmp_path, name="ThreadCreate")
  thread_mgr = ThreadManager(_cfg)
  sidebar_state.reset_for_tests()

  await thread_mgr.create_thread(session, "work")

  assert sidebar_state.is_dirty(session.id)


@pytest.mark.asyncio
async def test_thread_update_status_marks_session_dirty(tmp_path: Path) -> None:
  _cfg, mgr, session = await make_home_session(tmp_path, name="ThreadStatus")
  thread_mgr = ThreadManager(_cfg)
  thread = await thread_mgr.create_thread(session, "work")
  sidebar_state.reset_for_tests()

  await thread_mgr.update_status(session.id, thread.id, ThreadStatus.RUNNING)

  assert sidebar_state.is_dirty(session.id)


@pytest.mark.asyncio
async def test_thread_save_metadata_marks_session_dirty(tmp_path: Path) -> None:
  _cfg, mgr, session = await make_home_session(tmp_path, name="ThreadSave")
  thread_mgr = ThreadManager(_cfg)
  thread = await thread_mgr.create_thread(session, "work")
  sidebar_state.reset_for_tests()

  await thread_mgr.save_metadata(thread)

  assert sidebar_state.is_dirty(session.id)


@pytest.mark.asyncio
async def test_trigger_save_marks_session_dirty(tmp_path: Path) -> None:
  _cfg, mgr, session = await make_home_session(tmp_path, name="Trigger")
  trigger_mgr = TriggerManager(_cfg, mgr)
  trigger = PendingTrigger(
      id="pending-dirty", session_id=session.id, fire_at=datetime.now(UTC) + timedelta(minutes=5), message="wake")
  sidebar_state.reset_for_tests()

  await trigger_mgr._save_trigger(trigger)

  assert sidebar_state.is_dirty(session.id)


@pytest.mark.asyncio
async def test_plan_save_marks_session_dirty(tmp_path: Path) -> None:
  _cfg, mgr, session = await make_home_session(tmp_path, name="Plans")
  plans_mgr = PlanRegistryManager(_cfg, mgr)
  sidebar_state.reset_for_tests()

  await plans_mgr._save(session.id, {"plans": []})

  assert sidebar_state.is_dirty(session.id)


def test_thinking_mark_busy_marks_session_dirty() -> None:
  mark_busy("sid-busy-mark")

  assert sidebar_state.is_dirty("sid-busy-mark")
  clear_busy("sid-busy-mark")


def test_thinking_clear_busy_marks_session_dirty() -> None:
  mark_busy("sid-busy-clear")
  sidebar_state.reset_for_tests()

  clear_busy("sid-busy-clear")

  assert sidebar_state.is_dirty("sid-busy-clear")


@pytest.mark.asyncio
async def test_fork_marks_child_session_dirty(tmp_path: Path) -> None:
  cfg, mgr, parent = await make_home_session(tmp_path, name="Parent")
  append_events(mgr.get_chat_events_path(parent.id), [{"type": "user", "content": "e0"}])
  sidebar_state.reset_for_tests()

  child = await mgr.fork_session(parent.id)

  assert sidebar_state.is_dirty(child.id)


# ---------------------------------------------------------------------------
# (b) equivalence — first poll and forced poll == direct probing
# ---------------------------------------------------------------------------


async def _expected_status(mgr, session_id: str, *, archived: bool) -> dict:
  """Direct-probe ground truth for one session (archived: constant-False shortcut)."""
  meta = await mgr.get_session(session_id)
  assert meta is not None
  if archived:
    return {
        "has_unread": bool(meta.has_unread),
        "has_running_tasks": False,
        "thinking_since": meta.thinking_since.isoformat() if meta.thinking_since else None,
        "has_pending_trigger": False,
        "pending_trigger_count": 0,
        "next_trigger_at": None,
        "has_pending_plan_approval": False,
    }
  running = await mgr._has_running_tasks(session_id)
  pending_count, next_trigger_at = await asyncio.to_thread(
      sessions_core.pending_trigger_state_sync,
      mgr._session_dir(session_id) / "triggers")
  has_plan = await asyncio.to_thread(
      sessions_core.has_pending_plan_approval_sync,
      mgr._session_dir(session_id) / "plans.json", session_id)
  return {
      "has_unread": bool(meta.has_unread),
      "has_running_tasks": bool(meta.thinking_since) or running,
      "thinking_since": meta.thinking_since.isoformat() if meta.thinking_since else None,
      "has_pending_trigger": pending_count > 0,
      "pending_trigger_count": pending_count,
      "next_trigger_at": next_trigger_at.isoformat() if next_trigger_at else None,
      "has_pending_plan_approval": has_plan,
  }


@pytest.mark.asyncio
async def test_first_and_forced_poll_match_direct_probing(tmp_path: Path) -> None:
  cfg, mgr, busy = await make_home_session(tmp_path, name="Busy")
  clean = await mgr.create_session(CreateSessionRequest(name="Clean"))
  archived = await mgr.create_session(CreateSessionRequest(name="Archived"))

  # busy: running thread + pending trigger + awaiting-approval plans + unread.
  write_thread_meta(cfg, busy.id, {"id": "t1", "status": "running"})
  now = datetime.now(UTC)
  write_trigger(
      cfg.sessions_dir / busy.id / "triggers" / "pending.json",
      PendingTrigger(id="pending", session_id=busy.id, fire_at=now + timedelta(minutes=3), message="wake"),
  )
  write_plans(
      cfg, busy.id, {
          "plans":
              [
                  {
                      "id": 1,
                      "title": "Plan",
                      "versions":
                          [
                              {
                                  "v": 1,
                                  "file": "artifacts/plan_01.html",
                                  "created_at": "2026-07-20T00:00:00+00:00",
                                  "trigger": "initial",
                                  "base": None,
                              }
                          ],
                      "takeoff": None,
                      "closed": None,
                  }
              ]
      })
  (cfg.sessions_dir / busy.id / "artifacts").mkdir(parents=True, exist_ok=True)
  (cfg.sessions_dir / busy.id / "artifacts" / "plan_01.html").write_text("<html></html>", encoding="utf-8")
  busy_meta = await mgr.get_session(busy.id)
  assert busy_meta is not None
  busy_meta.has_unread = True
  await mgr.save_metadata(busy_meta)

  # archived: leave running thread + trigger + plans on disk; the shortcut must win.
  write_thread_meta(cfg, archived.id, {"id": "t2", "status": "running"})
  write_trigger(
      cfg.sessions_dir / archived.id / "triggers" / "pending-archived.json",
      PendingTrigger(
          id="pending-archived", session_id=archived.id, fire_at=now + timedelta(minutes=4), message="archived"),
  )
  archived_meta = await mgr.get_session(archived.id)
  assert archived_meta is not None
  archived_meta.status = SessionStatus.ARCHIVED
  await mgr.save_metadata(archived_meta)

  ids = f"{busy.id},{clean.id},{archived.id}"
  # Cold start: the fixture setup's writes marked sessions dirty, so clear the
  # registry — no snapshot entry, no dirty mark, the first poll is a full probe.
  sidebar_state.reset_for_tests()
  first = await sessions_api.all_sessions_status(ids=ids, session_mgr=mgr)
  forced = await sessions_api.all_sessions_status(ids=ids, session_mgr=mgr, force=True)

  expected = {
      busy.id: await _expected_status(mgr, busy.id, archived=False),
      clean.id: await _expected_status(mgr, clean.id, archived=False),
      archived.id: await _expected_status(mgr, archived.id, archived=True),
  }
  assert first == expected
  assert forced == expected


@pytest.mark.asyncio
async def test_dirty_session_is_reprobed_and_snapshot_refreshed(tmp_path: Path) -> None:
  cfg, mgr, session = await make_home_session(tmp_path, name="BecomesBusy")
  thread_mgr = ThreadManager(cfg)

  first = await sessions_api.all_sessions_status(ids=session.id, session_mgr=mgr)
  assert first[session.id]["has_running_tasks"] is False

  thread = await thread_mgr.create_thread(session, "work")
  await thread_mgr.update_status(session.id, thread.id, ThreadStatus.RUNNING)
  assert sidebar_state.is_dirty(session.id)

  second = await sessions_api.all_sessions_status(ids=session.id, session_mgr=mgr)
  assert second[session.id]["has_running_tasks"] is True

  # The re-probe consumed the dirty mark: the next poll is clean but still current.
  assert not sidebar_state.is_dirty(session.id)
  third = await sessions_api.all_sessions_status(ids=session.id, session_mgr=mgr)
  assert third == second


# ---------------------------------------------------------------------------
# (c) no-work clean path, the force param, and the every-10th fallback
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_clean_second_poll_does_no_probe_work(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  _cfg, mgr, session = await make_home_session(tmp_path, name="Quiet")

  first = await sessions_api.all_sessions_status(ids=session.id, session_mgr=mgr)

  calls = _counting_probes(monkeypatch)
  second = await sessions_api.all_sessions_status(ids=session.id, session_mgr=mgr)

  assert calls == {"running": 0, "trigger": 0, "plan": 0}
  assert second == first


@pytest.mark.asyncio
async def test_force_param_reprobes_clean_sessions(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  _cfg, mgr, session = await make_home_session(tmp_path, name="Forced")

  await sessions_api.all_sessions_status(ids=session.id, session_mgr=mgr)

  calls = _counting_probes(monkeypatch)
  forced = await sessions_api.all_sessions_status(ids=session.id, session_mgr=mgr, force=True)

  assert calls == {"running": 1, "trigger": 1, "plan": 1}
  assert forced[session.id]["has_running_tasks"] is False


@pytest.mark.asyncio
async def test_every_tenth_poll_is_stat_only_when_probe_inputs_unchanged(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, mgr, first = await make_home_session(tmp_path, name="A")
  second = await mgr.create_session(CreateSessionRequest(name="B"))
  write_thread_meta(cfg, second.id, {"id": "t1", "status": "running"})

  ids = f"{first.id},{second.id}"
  # Poll 1 (cold) probes both and warms the snapshot.
  await sessions_api.all_sessions_status(ids=ids, session_mgr=mgr)

  calls = _counting_probes(monkeypatch)
  for _ in range(8):  # polls 2-9: clean -> zero probe work
    await sessions_api.all_sessions_status(ids=ids, session_mgr=mgr)
  assert calls == {"running": 0, "trigger": 0, "plan": 0}

  # Poll 10 sweeps every active session, but the stat-only probe-input
  # signature is unchanged, so no session pays the deep read+parse.
  await sessions_api.all_sessions_status(ids=ids, session_mgr=mgr)
  assert calls == {"running": 0, "trigger": 0, "plan": 0}


@pytest.mark.asyncio
async def test_every_tenth_poll_heals_external_write_without_dirty_mark(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, mgr, first = await make_home_session(tmp_path, name="A")
  second = await mgr.create_session(CreateSessionRequest(name="B"))

  ids = f"{first.id},{second.id}"
  await sessions_api.all_sessions_status(ids=ids, session_mgr=mgr)  # poll 1 (cold)

  # An out-of-funnel writer (no mark_sidebar_dirty): the self-heal sweep must
  # still pick the file change up, and only for the session that changed.
  write_thread_meta(cfg, second.id, {"id": "t1", "status": "running"})

  calls = _counting_probes(monkeypatch)
  for _ in range(8):  # polls 2-9: no dirty mark -> still the stale snapshot
    await sessions_api.all_sessions_status(ids=ids, session_mgr=mgr)
  assert calls == {"running": 0, "trigger": 0, "plan": 0}

  healed = await sessions_api.all_sessions_status(ids=ids, session_mgr=mgr)  # poll 10
  assert calls == {"running": 1, "trigger": 1, "plan": 1}
  assert healed[second.id]["has_running_tasks"] is True


@pytest.mark.asyncio
async def test_scan_window_rollover_reprobes_without_file_change(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, mgr, session = await make_home_session(tmp_path, name="Aging")
  write_thread_meta(cfg, session.id, {"id": "t1", "status": "running"})
  first = await sessions_api.all_sessions_status(ids=session.id, session_mgr=mgr)
  assert first[session.id]["has_running_tasks"] is True

  # Jump the clock past the 30-day scan window with no file changing. Polls
  # 2-9 serve the stale snapshot; the poll-10 sweep must re-probe anyway
  # because the signature's rollover element can't vouch beyond it.
  future = datetime.now(UTC) + timedelta(days=31)
  monkeypatch.setattr(sessions_core, "utc_now", lambda: future)
  monkeypatch.setattr(sessions_core.time, "time", lambda: future.timestamp())
  for _ in range(8):
    interim = await sessions_api.all_sessions_status(ids=session.id, session_mgr=mgr)
  assert interim[session.id]["has_running_tasks"] is True

  tenth = await sessions_api.all_sessions_status(ids=session.id, session_mgr=mgr)
  assert tenth[session.id]["has_running_tasks"] is False
