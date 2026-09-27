"""Crash-recovery reports route through deliver_to_successor.

The startup recovery pass persists an unattended chat-event write; it must land
in the session that currently ends the succession chain. Events rerouted to a
different session carry ``origin_session_id``; a session with no successor keeps
writing into itself with no origin stamp.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any
from unittest.mock import AsyncMock, patch

import pytest
from conftest import (
    BROADCAST_PATCH_TARGET,
    build_worktree_cfg,
)
from conftest import make_parent as _make_parent

from src.core.init import _report_recovery_event
from src.core.sessions import SessionManager


def _broadcast_patch() -> Any:
  return patch(BROADCAST_PATCH_TARGET, new=AsyncMock())


async def _elone(mgr: SessionManager, parent_id: str) -> str:
  return (await mgr.elone_session(parent_id, event_index=0)).id


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


@pytest.mark.asyncio
async def test_crash_recovery_report_no_successor_writes_into_itself_without_origin(tmp_path: Path) -> None:
  mgr = SessionManager(build_worktree_cfg(tmp_path))
  session_id = await _make_parent(mgr)

  with _broadcast_patch():
    await _report_recovery_event(mgr, session_id, "worker thread stalled")

  own_events = mgr.load_chat_events_sync(session_id)
  report = next(ev for ev in own_events if ev.get("source") == "crash_recovery")
  assert "worker thread stalled" in report["content"]
  assert "origin_session_id" not in report
