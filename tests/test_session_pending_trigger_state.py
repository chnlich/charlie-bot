from __future__ import annotations

import json
from datetime import datetime, timedelta, timezone
from pathlib import Path

import pytest

from src.api import sessions as sessions_api
from src.core.config import CharlieBotConfig
from src.core.models import CreateSessionRequest, PendingTrigger, SessionStatus, TriggerStatus
from src.core.sessions import SessionManager


def _write_trigger(path: Path, trigger: PendingTrigger) -> None:
  path.parent.mkdir(parents=True, exist_ok=True)
  path.write_text(trigger.model_dump_json(indent=2), encoding="utf-8")


@pytest.mark.asyncio
async def test_pending_trigger_state_is_derived_without_persisting_metadata(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "charliebot-home")
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="Wake later"))

  now = datetime.now(timezone.utc)
  triggers_dir = cfg.sessions_dir / session.id / "triggers"
  _write_trigger(
      triggers_dir / "pending-late.json",
      PendingTrigger(
          id="pending-late",
          session_id=session.id,
          fire_at=now + timedelta(minutes=10),
          message="later",
      ),
  )
  _write_trigger(
      triggers_dir / "pending-soon.json",
      PendingTrigger(
          id="pending-soon",
          session_id=session.id,
          fire_at=now + timedelta(minutes=5),
          message="soon",
      ),
  )
  _write_trigger(
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
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "charliebot-home")
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="Wake later"))

  meta = await session_mgr.get_session(session.id)
  assert meta is not None
  meta.has_unread = True
  await session_mgr._save_metadata(meta)

  now = datetime.now(timezone.utc)
  trigger = PendingTrigger(
      id="pending-status",
      session_id=session.id,
      fire_at=now + timedelta(minutes=3),
      message="status",
  )
  _write_trigger(cfg.sessions_dir / session.id / "triggers" / "pending-status.json", trigger)

  status = await sessions_api.all_sessions_status(session_mgr=session_mgr)
  assert status[session.id]["has_unread"] is True
  assert status[session.id]["has_running_tasks"] is False
  assert status[session.id]["has_pending_trigger"] is True
  assert status[session.id]["pending_trigger_count"] == 1
  assert status[session.id]["next_trigger_at"] == trigger.fire_at.isoformat()


@pytest.mark.asyncio
async def test_all_sessions_status_includes_archived_sessions(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "charliebot-home")
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="Archived wake"))

  meta = await session_mgr.get_session(session.id)
  assert meta is not None
  meta.status = SessionStatus.ARCHIVED
  await session_mgr._save_metadata(meta)

  trigger = PendingTrigger(
      id="pending-archived",
      session_id=session.id,
      fire_at=datetime.now(timezone.utc) + timedelta(minutes=2),
      message="archived status",
  )
  _write_trigger(cfg.sessions_dir / session.id / "triggers" / "pending-archived.json", trigger)

  status = await sessions_api.all_sessions_status(session_mgr=session_mgr)
  assert status[session.id]["has_pending_trigger"] is True
  assert status[session.id]["pending_trigger_count"] == 1
