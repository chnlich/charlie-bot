"""GET /api/sessions/{id}/pending-triggers: the chat column tray's payload.

The endpoint is the one trigger-list API: pending records only, fire_at
ascending, each element the record's own JSON form; an unknown session 404s.
"""

import asyncio
import json
from datetime import UTC, datetime, timedelta
from pathlib import Path

from conftest import fake_backends
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.api.deps import get_session_manager, get_trigger_manager
from src.api.sessions import router as sessions_router
from src.core.config import CharlieBotConfig
from src.core.models import CreateSessionRequest, PendingTrigger, TriggerStatus
from src.core.sessions import SessionManager
from src.core.triggers import TriggerManager


def _client(tmp_path: Path) -> tuple[TestClient, CharlieBotConfig, SessionManager, str]:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home", backends=fake_backends())
  sessions = SessionManager(cfg)

  async def seed() -> str:
    session = await sessions.create_session(CreateSessionRequest(name="tray"))
    return session.id

  session_id = asyncio.run(seed())
  app = FastAPI()
  app.include_router(sessions_router, prefix="/api/sessions")
  app.dependency_overrides[get_trigger_manager] = lambda: TriggerManager(cfg, sessions)
  app.dependency_overrides[get_session_manager] = lambda: sessions
  return TestClient(app), cfg, sessions, session_id


def _save_trigger(
    mgr: TriggerManager,
    session_id: str,
    *,
    minutes: int,
    message: str,
    status: TriggerStatus = TriggerStatus.PENDING) -> PendingTrigger:
  trigger = PendingTrigger(
      session_id=session_id,
      fire_at=datetime.now(UTC) + timedelta(minutes=minutes),
      message=message,
      watch_targets=[],
      status=status,
  )
  asyncio.run(mgr._save_trigger(trigger))
  return trigger


def test_pending_only_sorted_ascending_in_record_json_form(tmp_path: Path) -> None:
  client, _cfg, sessions, session_id = _client(tmp_path)
  mgr = TriggerManager(CharlieBotConfig(charliebot_home=tmp_path / "home", backends=fake_backends()), sessions)
  late = _save_trigger(mgr, session_id, minutes=120, message="later")
  early = _save_trigger(mgr, session_id, minutes=30, message="earlier")
  _save_trigger(mgr, session_id, minutes=10, message="already fired", status=TriggerStatus.FIRED)
  _save_trigger(mgr, session_id, minutes=5, message="already cancelled", status=TriggerStatus.CANCELLED)

  res = client.get(f"/api/sessions/{session_id}/pending-triggers")

  assert res.status_code == 200
  body = res.json()
  assert [row["id"] for row in body] == [early.id, late.id]
  for row, record in zip(body, [early, late], strict=True):
    assert row == json.loads(record.model_dump_json())


def test_watch_targets_and_record_fields_round_trip(tmp_path: Path) -> None:
  client, _cfg, sessions, session_id = _client(tmp_path)
  mgr = TriggerManager(CharlieBotConfig(charliebot_home=tmp_path / "home", backends=fake_backends()), sessions)
  trigger = PendingTrigger(
      session_id=session_id,
      fire_at=datetime.now(UTC) + timedelta(hours=1),
      message="watched",
      watch_targets=[{
          "kind": "local_pid",
          "pid": 12345
      }],
  )
  asyncio.run(mgr._save_trigger(trigger))

  body = client.get(f"/api/sessions/{session_id}/pending-triggers").json()

  assert body == [json.loads(trigger.model_dump_json())]
  assert body[0]["watch_targets"] == [{"kind": "local_pid", "pid": 12345}]
  assert body[0]["status"] == "pending"
  assert body[0]["fired_at"] is None and body[0]["fire_reason"] is None


def test_unknown_session_answers_404(tmp_path: Path) -> None:
  client, _cfg, _sessions, _session_id = _client(tmp_path)

  res = client.get("/api/sessions/no-such-session/pending-triggers")

  assert res.status_code == 404
  assert res.json()["detail"] == "Session not found"


def test_session_without_triggers_answers_empty_array(tmp_path: Path) -> None:
  client, _cfg, _sessions, session_id = _client(tmp_path)

  res = client.get(f"/api/sessions/{session_id}/pending-triggers")

  assert res.status_code == 200
  assert res.json() == []
