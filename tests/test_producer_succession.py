"""Stage C: unattended chat-event producers route through deliver_to_successor.

Every site that persists an unattended chat-event write — delegation finalization,
improve-loop reporting, review-chain cleanup, and crash-recovery reporting — must
land in the session that currently ends the succession chain. Events rerouted to a
different session carry ``origin_session_id``; a session with no successor keeps
writing into itself with no origin stamp.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
from conftest import (
    BROADCAST_PATCH_TARGET,
    OPUS_BACKEND_ID,
    OPUS_BACKEND_OPTION,
    build_worktree_cfg,
    patch_improve_git_ops,
)
from conftest import make_parent as _make_parent

from src.core import event_types as ET
from src.core import improve_command
from src.core.improve_command import run_improve_loop
from src.core.init import _report_recovery_event
from src.core.models import ThreadMetadata
from src.core.sessions import SessionManager
from src.core.spawner_events import _thread_worker_event
from src.core.spawner_finalize import _persist_worker_summary_once


def _broadcast_patch() -> Any:
  return patch(BROADCAST_PATCH_TARGET, new=AsyncMock())


async def _elone(mgr: SessionManager, parent_id: str) -> str:
  return (await mgr.elone_session(parent_id, event_index=0)).id


# ---------------------------------------------------------------------------
# spawner_finalize: worker_summary delivery
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_spawner_worker_summary_lands_in_successor_and_preserves_thread_id(tmp_path: Path) -> None:
  mgr = SessionManager(build_worktree_cfg(tmp_path))
  parent_id = await _make_parent(mgr)
  child_id = await _elone(mgr, parent_id)

  thread = ThreadMetadata(session_id=parent_id, id="thread-1", description="delegate", backend=OPUS_BACKEND_ID)
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
  mgr = SessionManager(build_worktree_cfg(tmp_path))
  session_id = await _make_parent(mgr)

  thread = ThreadMetadata(session_id=session_id, id="thread-1", description="hello", backend=OPUS_BACKEND_ID)
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
  mgr = SessionManager(build_worktree_cfg(tmp_path))
  parent_id = await _make_parent(mgr)
  child_id = await _elone(mgr, parent_id)

  with _broadcast_patch():
    await _report_recovery_event(mgr, parent_id, "worker thread ended with descendant procs")

  child_events = mgr.load_chat_events_sync(child_id)
  report = next(ev for ev in child_events if ev.get("source") == "crash_recovery")
  assert report["origin_session_id"] == parent_id
  assert "descendant procs" in report["content"]


# ---------------------------------------------------------------------------
# improve loop: final summary and worktree-creation failure
# ---------------------------------------------------------------------------


async def _run_succession_loop(
    tmp_path: Path,
    mgr: SessionManager,
    session_id: str,
    *,
    iterations: int,
    repo_name: str,
) -> None:
  """run_improve_loop on the succession tests' shared call tail: goal ``tune``,
  ``improve/test`` off ``main``, the OPUS backend, ``merge_back=False``."""
  with _broadcast_patch():
    await run_improve_loop(
        session_id=session_id,
        repo_path=str(tmp_path / repo_name),
        iterations=iterations,
        goal="tune",
        cfg=build_worktree_cfg(tmp_path),
        session_mgr=mgr,
        thread_mgr=MagicMock(),
        base_branch="main",
        work_branch="improve/test",
        merge_back=False,
        resolved_backend=OPUS_BACKEND_ID,
        resolved_model=OPUS_BACKEND_OPTION.model,
    )


@pytest.mark.asyncio
async def test_improve_final_summary_lands_in_successor(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  mgr = SessionManager(build_worktree_cfg(tmp_path))
  parent_id = await _make_parent(mgr)
  child_id = await _elone(mgr, parent_id)

  patch_improve_git_ops(monkeypatch)
  monkeypatch.setattr(improve_command, "trigger_master", AsyncMock())
  await _run_succession_loop(tmp_path, mgr, parent_id, iterations=0, repo_name="repo")

  child_events = mgr.load_chat_events_sync(child_id)
  summary = next(ev for ev in child_events if ev.get("type") == ET.IMPROVE_COMPLETED)
  assert summary["origin_session_id"] == parent_id
  assert summary["goal"] == "tune"


# ---------------------------------------------------------------------------
# review chain: cleanup error
