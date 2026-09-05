from __future__ import annotations

import json
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest
from conftest import make_home_config, write_trigger

from src.api import sessions as sessions_api
from src.core.models import (
    CreateSessionRequest,
    PendingTrigger,
    SessionStatus,
    TriggerStatus,
)
from src.core.sessions import SessionManager


@pytest.mark.asyncio
async def test_pending_trigger_state_is_derived_without_persisting_metadata(tmp_path: Path) -> None:
  cfg = make_home_config(tmp_path)
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="Wake later"))

  now = datetime.now(UTC)
  triggers_dir = cfg.sessions_dir / session.id / "triggers"
  write_trigger(
      triggers_dir / "pending-late.json",
      PendingTrigger(
          id="pending-late",
          session_id=session.id,
          fire_at=now + timedelta(minutes=10),
          message="later",
      ),
  )
  write_trigger(
      triggers_dir / "pending-soon.json",
      PendingTrigger(
          id="pending-soon",
          session_id=session.id,
          fire_at=now + timedelta(minutes=5),
          message="soon",
      ),
  )
  write_trigger(
      triggers_dir / "cancelled.json",
      PendingTrigger(
          id="cancelled",
          session_id=session.id,
          fire_at=now + timedelta(minutes=1),
          message="cancelled",
          status=TriggerStatus.CANCELLED,
      ),
  )

  listed = await session_mgr.list_sessions(
      status=SessionStatus.ACTIVE,
      include_pending_trigger_status=True,
  )
  assert len(listed) == 1
  assert listed[0].has_pending_trigger is True
  assert listed[0].pending_trigger_count == 2
  assert listed[0].next_trigger_at == now + timedelta(minutes=5)

  searched = await session_mgr.search_sessions(
      "Wake later",
      include_pending_trigger_status=True,
  )
  assert len(searched) == 1
  assert searched[0].has_pending_trigger is True
  assert searched[0].pending_trigger_count == 2

  fresh = await session_mgr.get_session(session.id)
  assert fresh is not None
  assert fresh.has_pending_trigger is False
  assert fresh.pending_trigger_count == 0
  assert fresh.next_trigger_at is None

  raw_metadata = json.loads((cfg.sessions_dir / session.id / "metadata.json").read_text(encoding="utf-8"))
  assert "has_pending_trigger" not in raw_metadata
  assert "pending_trigger_count" not in raw_metadata
  assert "next_trigger_at" not in raw_metadata


@pytest.mark.asyncio
async def test_all_sessions_status_includes_pending_trigger_fields(tmp_path: Path) -> None:
  cfg = make_home_config(tmp_path)
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="Wake later"))

  meta = await session_mgr.get_session(session.id)
  assert meta is not None
  meta.has_unread = True
  await session_mgr.save_metadata(meta)

  now = datetime.now(UTC)
  trigger = PendingTrigger(
      id="pending-status",
      session_id=session.id,
      fire_at=now + timedelta(minutes=3),
      message="status",
  )
  write_trigger(cfg.sessions_dir / session.id / "triggers" / "pending-status.json", trigger)

  status = json.loads((await sessions_api.all_sessions_status(ids=session.id, session_mgr=session_mgr)).body)
  assert status[session.id]["has_unread"] is True
  assert status[session.id]["has_running_tasks"] is False
  assert status[session.id]["has_pending_trigger"] is True
  assert status[session.id]["pending_trigger_count"] == 1
  assert status[session.id]["next_trigger_at"] == trigger.fire_at.isoformat()


@pytest.mark.asyncio
async def test_all_sessions_status_includes_archived_sessions(tmp_path: Path) -> None:
  cfg = make_home_config(tmp_path)
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="Archived wake"))

  meta = await session_mgr.get_session(session.id)
  assert meta is not None
  meta.status = SessionStatus.ARCHIVED
  await session_mgr.save_metadata(meta)

  trigger = PendingTrigger(
      id="pending-archived",
      session_id=session.id,
      fire_at=datetime.now(UTC) + timedelta(minutes=2),
      message="archived status",
  )
  write_trigger(cfg.sessions_dir / session.id / "triggers" / "pending-archived.json", trigger)

  status = json.loads((await sessions_api.all_sessions_status(ids=session.id, session_mgr=session_mgr)).body)
  # Archived sessions remain in the status response but skip per-session
  # filesystem work, so pending-trigger fields are always empty regardless of
  # any trigger files still on disk.
  assert session.id in status
  assert status[session.id]["has_running_tasks"] is False
  assert status[session.id]["has_pending_trigger"] is False
  assert status[session.id]["pending_trigger_count"] == 0
  assert status[session.id]["next_trigger_at"] is None


@pytest.mark.asyncio
async def test_populate_sidebar_state_skips_archived_sessions(tmp_path: Path) -> None:
  cfg = make_home_config(tmp_path)
  session_mgr = SessionManager(cfg)

  active = await session_mgr.create_session(CreateSessionRequest(name="Active"))
  archived = await session_mgr.create_session(CreateSessionRequest(name="Archived"))

  archived_meta = await session_mgr.get_session(archived.id)
  assert archived_meta is not None
  archived_meta.status = SessionStatus.ARCHIVED
  await session_mgr.save_metadata(archived_meta)

  now = datetime.now(UTC)
  write_trigger(
      cfg.sessions_dir / active.id / "triggers" / "pending-active.json",
      PendingTrigger(
          id="pending-active",
          session_id=active.id,
          fire_at=now + timedelta(minutes=4),
          message="active",
      ),
  )
  write_trigger(
      cfg.sessions_dir / archived.id / "triggers" / "pending-archived.json",
      PendingTrigger(
          id="pending-archived",
          session_id=archived.id,
          fire_at=now + timedelta(minutes=4),
          message="archived",
      ),
  )

  # Refuse to load archived running-task state from disk. If
  # populate_sidebar_state probes archived sessions through the manager,
  # the test fails. The archived trigger file written above pins the
  # constant-False shortcut through the assertions below.
  original_has_running = session_mgr._has_running_tasks

  async def _fail_for_archived_running(session_id: str):
    if session_id == archived.id:
      raise AssertionError("archived session should not query running tasks")
    return await original_has_running(session_id)

  session_mgr._has_running_tasks = _fail_for_archived_running

  active_fresh = await session_mgr.get_session(active.id)
  archived_fresh = await session_mgr.get_session(archived.id)
  assert active_fresh is not None
  assert archived_fresh is not None
  # Preserve input list order to validate it is also preserved on output.
  sessions = [active_fresh, archived_fresh]

  await session_mgr.populate_sidebar_state(
      sessions,
      include_running_status=True,
      include_pending_trigger_status=True,
  )

  assert [s.id for s in sessions] == [active.id, archived.id]

  assert active_fresh.has_running_tasks is False
  assert active_fresh.has_pending_trigger is True
  assert active_fresh.pending_trigger_count == 1
  assert active_fresh.next_trigger_at is not None

  assert archived_fresh.has_running_tasks is False
  assert archived_fresh.has_pending_trigger is False
  assert archived_fresh.pending_trigger_count == 0
  assert archived_fresh.next_trigger_at is None
