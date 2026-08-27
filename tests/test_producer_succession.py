"""Stage C: unattended chat-event producers route through deliver_to_successor.

Every site that persists an unattended chat-event write — delegation finalization,
improve-loop reporting, review-chain cleanup, and crash-recovery reporting — must
land in the session that currently ends the succession chain. Events rerouted to a
different session carry ``origin_session_id``; a session with no successor keeps
writing into itself with no origin stamp.
"""

from __future__ import annotations

from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
from conftest import BROADCAST_PATCH_TARGET, patch_improve_git_ops
from conftest import make_parent as _make_parent

from src.core import event_types as ET
from src.core import improve_command, review
from src.core.config import CharlieBotConfig
from src.core.improve_command import run_improve_loop
from src.core.init import _report_recovery_event
from src.core.models import ThreadMetadata
from src.core.sessions import SessionManager
from src.core.spawner_events import _thread_worker_event
from src.core.spawner_finalize import _persist_worker_summary_once


def _make_cfg(tmp_path: Path) -> CharlieBotConfig:
  return CharlieBotConfig(charliebot_home=tmp_path / "home")


def _broadcast_patch():
  return patch(BROADCAST_PATCH_TARGET, new=AsyncMock())


async def _elone(mgr: SessionManager, parent_id: str) -> str:
  return (await mgr.elone_session(parent_id, event_index=0)).id


# ---------------------------------------------------------------------------
# spawner_finalize: worker_summary delivery
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_spawner_worker_summary_lands_in_successor_and_preserves_thread_id(tmp_path: Path) -> None:
  mgr = SessionManager(_make_cfg(tmp_path))
  parent_id = await _make_parent(mgr)
  child_id = await _elone(mgr, parent_id)

  thread = ThreadMetadata(session_id=parent_id, id="thread-1", description="delegate", backend="claude-opus-4.6")
  event = _thread_worker_event(thread, "completed", full_content="done", content="locator")

  with _broadcast_patch():
    await _persist_worker_summary_once(parent_id, thread.id, event, mgr, fallback=False)

  child_events = mgr.load_chat_events_sync(child_id)
  summary = next(ev for ev in child_events if ev.get("type") == ET.WORKER_SUMMARY)
  assert summary["thread_id"] == thread.id
  assert summary["origin_session_id"] == parent_id
  assert summary["content"] == "locator"


@pytest.mark.asyncio
async def test_spawner_worker_summary_no_successor_writes_into_itself_without_origin(tmp_path: Path) -> None:
  mgr = SessionManager(_make_cfg(tmp_path))
  session_id = await _make_parent(mgr)

  thread = ThreadMetadata(session_id=session_id, id="thread-1", description="hello", backend="claude-opus-4.6")
  event = _thread_worker_event(thread, "completed", full_content="done", content="locator")

  with _broadcast_patch():
    await _persist_worker_summary_once(session_id, thread.id, event, mgr, fallback=False)

  own_events = mgr.load_chat_events_sync(session_id)
  summary = next(ev for ev in own_events if ev.get("type") == ET.WORKER_SUMMARY)
  assert summary["thread_id"] == thread.id
  assert "origin_session_id" not in summary


# ---------------------------------------------------------------------------
# init: crash-recovery report
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_crash_recovery_report_lands_in_successor(tmp_path: Path) -> None:
  mgr = SessionManager(_make_cfg(tmp_path))
  parent_id = await _make_parent(mgr)
  child_id = await _elone(mgr, parent_id)

  with _broadcast_patch():
    await _report_recovery_event(mgr, parent_id, "worker thread ended with descendant procs")

  child_events = mgr.load_chat_events_sync(child_id)
  report = next(ev for ev in child_events if ev.get("source") == "crash_recovery")
  assert report["origin_session_id"] == parent_id
  assert "descendant procs" in report["content"]


@pytest.mark.asyncio
async def test_crash_recovery_report_no_successor_writes_into_itself_without_origin(tmp_path: Path) -> None:
  mgr = SessionManager(_make_cfg(tmp_path))
  session_id = await _make_parent(mgr)

  with _broadcast_patch():
    await _report_recovery_event(mgr, session_id, "worker thread stalled")

  own_events = mgr.load_chat_events_sync(session_id)
  report = next(ev for ev in own_events if ev.get("source") == "crash_recovery")
  assert "worker thread stalled" in report["content"]
  assert "origin_session_id" not in report


# ---------------------------------------------------------------------------
# improve loop: final summary and worktree-creation failure
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_improve_final_summary_lands_in_successor(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  mgr = SessionManager(_make_cfg(tmp_path))
  parent_id = await _make_parent(mgr)
  child_id = await _elone(mgr, parent_id)

  patch_improve_git_ops(monkeypatch)
  monkeypatch.setattr(improve_command, "trigger_master", AsyncMock())

  with _broadcast_patch():
    await run_improve_loop(
        session_id=parent_id,
        repo_path=str(tmp_path / "repo"),
        iterations=0,
        goal="tune",
        cfg=_make_cfg(tmp_path),
        session_mgr=mgr,
        thread_mgr=MagicMock(),
        base_branch="main",
        work_branch="improve/test",
        merge_back=False,
        resolved_backend="claude-opus-4.6",
        resolved_model="claude-opus-4-6",
    )

  child_events = mgr.load_chat_events_sync(child_id)
  summary = next(ev for ev in child_events if ev.get("type") == ET.IMPROVE_COMPLETED)
  assert summary["origin_session_id"] == parent_id
  assert summary["goal"] == "tune"


@pytest.mark.asyncio
async def test_improve_worktree_creation_failure_lands_in_successor(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  mgr = SessionManager(_make_cfg(tmp_path))
  parent_id = await _make_parent(mgr)
  child_id = await _elone(mgr, parent_id)

  async def fake_fail_create_worktree(repo_path: Path, base_branch: str, branch_name: str, wt_path: Path):
    del repo_path, base_branch, branch_name, wt_path
    raise RuntimeError("no such repo")

  monkeypatch.setattr(improve_command, "git_create_worktree", fake_fail_create_worktree)
  monkeypatch.setattr(improve_command, "trigger_master", AsyncMock())

  with _broadcast_patch():
    await run_improve_loop(
        session_id=parent_id,
        repo_path=str(tmp_path / "missing"),
        iterations=3,
        goal="tune",
        cfg=_make_cfg(tmp_path),
        session_mgr=mgr,
        thread_mgr=MagicMock(),
        base_branch="main",
        work_branch="improve/test",
        merge_back=False,
        resolved_backend="claude-opus-4.6",
        resolved_model="claude-opus-4-6",
    )

  child_events = mgr.load_chat_events_sync(child_id)
  failure = next(ev for ev in child_events if ev.get("type") == ET.IMPROVE_FAILED)
  assert failure["origin_session_id"] == parent_id
  assert "Failed to create worktree" in failure["error"]


@pytest.mark.asyncio
async def test_improve_final_summary_no_successor_writes_into_itself_without_origin(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  mgr = SessionManager(_make_cfg(tmp_path))
  session_id = await _make_parent(mgr)

  patch_improve_git_ops(monkeypatch)
  monkeypatch.setattr(improve_command, "trigger_master", AsyncMock())

  with _broadcast_patch():
    await run_improve_loop(
        session_id=session_id,
        repo_path=str(tmp_path / "repo"),
        iterations=0,
        goal="tune",
        cfg=_make_cfg(tmp_path),
        session_mgr=mgr,
        thread_mgr=MagicMock(),
        base_branch="main",
        work_branch="improve/test",
        merge_back=False,
        resolved_backend="claude-opus-4.6",
        resolved_model="claude-opus-4-6",
    )

  own_events = mgr.load_chat_events_sync(session_id)
  summary = next(ev for ev in own_events if ev.get("type") == ET.IMPROVE_COMPLETED)
  assert summary["goal"] == "tune"
  assert "origin_session_id" not in summary


# ---------------------------------------------------------------------------
# review chain: cleanup error
# ---------------------------------------------------------------------------


def _patch_review_reviewer_chain(monkeypatch: pytest.MonkeyPatch, *, cleanup_error: str) -> None:
  async def fake_finalize_review_chain(session_id, original_thread, thread_mgr, worktree_parent) -> str:
    del session_id, original_thread, thread_mgr, worktree_parent
    return cleanup_error

  monkeypatch.setattr(review, "finalize_review_chain", fake_finalize_review_chain)
  monkeypatch.setattr(review, "_trigger_master_judged", AsyncMock())


def _make_review_thread_mgr(tmp_path: Path) -> MagicMock:
  events_path = tmp_path / "original-events.jsonl"
  events_path.write_text("")
  thread_mgr = MagicMock()
  thread_mgr.get_thread = AsyncMock(side_effect=lambda session, tid: MagicMock(review_of="original", id=tid))
  thread_mgr.get_events_log_path = AsyncMock(return_value=events_path)
  return thread_mgr


@pytest.mark.asyncio
async def test_review_cleanup_error_is_routed_to_successor(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The review-chain cleanup error write goes through deliver_to_successor.

  The site sits deep inside ``maybe_spawn_reviewer`` behind the full reviewer
  spawn machinery, so the migration is asserted by patching the primitive and
  checking it is invoked with the owning session id and the error event.
  """
  mgr = SessionManager(_make_cfg(tmp_path))
  parent_id = await _make_parent(mgr)
  child_id = await _elone(mgr, parent_id)

  real_deliver = mgr.deliver_to_successor

  async def fake_deliver(session, event):
    return await real_deliver(session, event)

  _patch_review_reviewer_chain(monkeypatch, cleanup_error="Worktree cleanup failed for /x: boom")
  thread_mgr = _make_review_thread_mgr(tmp_path)

  thread = MagicMock(id="reviewer-1", description="original", backend="claude-opus-4.6")
  with _broadcast_patch():
    with patch.object(mgr, "deliver_to_successor", side_effect=fake_deliver) as mock_deliver:
      await review.maybe_spawn_reviewer(
          parent_id,
          thread,
          exit_code=0,
          events_summary="events",
          full_summary="summary",
          thread_mgr=thread_mgr,
          session_mgr=mgr,
          cfg=_make_cfg(tmp_path),
      )

  mock_deliver.assert_awaited_once()
  called_session, called_event = mock_deliver.await_args.args
  assert called_session == parent_id
  assert called_event["type"] == ET.ERROR
  assert "boom" in called_event["content"]
  assert any(
      ev.get("type") == ET.ERROR and "boom" in ev.get("content", "") for ev in mgr.load_chat_events_sync(child_id))


@pytest.mark.asyncio
async def test_review_cleanup_error_no_successor_writes_into_itself_without_origin(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """For a session with no successor, the routed cleanup error lands in that session itself."""
  mgr = SessionManager(_make_cfg(tmp_path))
  session_id = await _make_parent(mgr)

  real_deliver = mgr.deliver_to_successor

  async def fake_deliver(session, event):
    return await real_deliver(session, event)

  _patch_review_reviewer_chain(monkeypatch, cleanup_error="Worktree cleanup for /x: boom")
  thread_mgr = _make_review_thread_mgr(tmp_path)

  thread = MagicMock(id="reviewer-1", description="original", backend="claude-opus-4.6")
  with _broadcast_patch():
    with patch.object(mgr, "deliver_to_successor", side_effect=fake_deliver) as mock_deliver:
      await review.maybe_spawn_reviewer(
          session_id,
          thread,
          exit_code=0,
          events_summary="events",
          full_summary="summary",
          thread_mgr=thread_mgr,
          session_mgr=mgr,
          cfg=_make_cfg(tmp_path),
      )

  mock_deliver.assert_awaited_once()
  own_events = mgr.load_chat_events_sync(session_id)
  error = next(ev for ev in own_events if ev.get("type") == ET.ERROR)
  assert "boom" in error["content"]
  assert "origin_session_id" not in error