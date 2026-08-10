"""Session Manager endpoint: resolve/create/heal state machine plus `role` round-trip."""

from __future__ import annotations

import asyncio
import json
from datetime import datetime, timedelta, timezone
from pathlib import Path

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.api import sessions as sessions_api
from src.api.deps import get_config, get_session_manager, get_trigger_manager
from src.core import thinking_state
from src.core.config import CharlieBotConfig
from src.core.models import (
  CreateSessionRequest,
  PendingTrigger,
  SessionMetadata,
  SessionStatus,
)
from src.core.sessions import SessionManager
from src.core.triggers import TriggerManager


@pytest.fixture
def orientation_sends(monkeypatch: pytest.MonkeyPatch) -> list[dict]:
  """Capture orientation-message sends and keep the master turn itself from running."""
  calls: list[dict] = []

  def fake_run_and_finalize(cfg, meta, content, session_mgr, **kwargs):
    calls.append({"session_id": meta.id, "content": content, "kwargs": kwargs})

    async def noop() -> None:
      return None

    return noop()

  def fake_create_logged_task(coro) -> None:
    coro.close()

  monkeypatch.setattr("src.api.chat.run_and_finalize", fake_run_and_finalize)
  monkeypatch.setattr("src.core.tasks.create_logged_task", fake_create_logged_task)
  return calls


def _write_trigger(cfg: CharlieBotConfig, trigger: PendingTrigger) -> None:
  path = cfg.sessions_dir / trigger.session_id / "triggers" / f"{trigger.id}.json"
  path.parent.mkdir(parents=True, exist_ok=True)
  path.write_text(trigger.model_dump_json(indent=2), encoding="utf-8")


def _patrol_trigger(session_id: str) -> PendingTrigger:
  return PendingTrigger(
      id="patrol-trigger",
      session_id=session_id,
      fire_at=datetime.now(timezone.utc) + timedelta(hours=4),
      message="[patrol] Patrol round: read prompts/session_manager.md",
  )


async def _create_manager(session_mgr: SessionManager) -> SessionMetadata:
  return await session_mgr.create_session(CreateSessionRequest(name="Session Manager", role="manager"))


async def _list_managers(session_mgr: SessionManager) -> list[SessionMetadata]:
  return [s for s in await session_mgr.list_sessions() if s.role == "manager"]


@pytest.mark.asyncio
async def test_manager_endpoint_creates_session_on_first_call(tmp_path: Path, orientation_sends: list) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "charliebot-home")
  session_mgr = SessionManager(cfg)
  trigger_mgr = TriggerManager(cfg, session_mgr)

  result = await sessions_api.get_manager_session(session_mgr=session_mgr, trigger_mgr=trigger_mgr, cfg=cfg)

  assert set(result.keys()) == {"id", "created", "patrol_armed"}
  assert result["created"] is True
  assert result["patrol_armed"] is False

  managers = await _list_managers(session_mgr)
  assert [m.id for m in managers] == [result["id"]]
  meta = managers[0]
  assert meta.name == "Session Manager"
  assert len(orientation_sends) == 1
  assert orientation_sends[0]["session_id"] == result["id"]
  assert orientation_sends[0]["content"] == sessions_api.MANAGER_ORIENTATION_MESSAGE


@pytest.mark.asyncio
async def test_manager_endpoint_reports_armed_patrol_without_sending(tmp_path: Path, orientation_sends: list) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "charliebot-home")
  session_mgr = SessionManager(cfg)
  trigger_mgr = TriggerManager(cfg, session_mgr)
  session = await _create_manager(session_mgr)
  _write_trigger(cfg, _patrol_trigger(session.id))

  result = await sessions_api.get_manager_session(session_mgr=session_mgr, trigger_mgr=trigger_mgr, cfg=cfg)

  assert result == {"id": session.id, "created": False, "patrol_armed": True}
  assert orientation_sends == []


@pytest.mark.asyncio
async def test_manager_endpoint_creates_new_session_when_manager_archived(
    tmp_path: Path,
    orientation_sends: list,
) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "charliebot-home")
  session_mgr = SessionManager(cfg)
  trigger_mgr = TriggerManager(cfg, session_mgr)
  old = await _create_manager(session_mgr)
  await session_mgr.archive_session(old.id)

  result = await sessions_api.get_manager_session(session_mgr=session_mgr, trigger_mgr=trigger_mgr, cfg=cfg)

  assert result["created"] is True
  assert result["id"] != old.id
  managers = await _list_managers(session_mgr)
  assert len(managers) == 2
  active_managers = [m for m in managers if m.status == SessionStatus.ACTIVE]
  assert [m.id for m in active_managers] == [result["id"]]
  assert len(orientation_sends) == 1
  assert orientation_sends[0]["session_id"] == result["id"]


@pytest.mark.asyncio
async def test_manager_endpoint_concurrent_calls_resolve_one_manager(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    orientation_sends: list,
) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "charliebot-home")
  session_mgr = SessionManager(cfg)
  trigger_mgr = TriggerManager(cfg, session_mgr)

  # Force the resolve/create race the module-level lock guards against: on an
  # empty store the resolve path runs suspension-free, so without this shim the
  # first call would finish creating before the second call even resolves, and
  # the test would pass even with the lock removed. Holding the first caller
  # back one loop tick lets the sibling resolve to "no manager" as well.
  original_list_sessions = SessionManager.list_sessions
  resolve_calls = 0

  async def yielding_list_sessions(self, *args, **kwargs):
    nonlocal resolve_calls
    resolve_calls += 1
    result = await original_list_sessions(self, *args, **kwargs)
    if resolve_calls == 1:
      await asyncio.sleep(0)
    return result

  monkeypatch.setattr(SessionManager, "list_sessions", yielding_list_sessions)

  first, second = await asyncio.gather(
      sessions_api.get_manager_session(session_mgr=session_mgr, trigger_mgr=trigger_mgr, cfg=cfg),
      sessions_api.get_manager_session(session_mgr=session_mgr, trigger_mgr=trigger_mgr, cfg=cfg),
  )

  assert first["id"] == second["id"]
  assert {first["created"], second["created"]} == {True, False}
  managers = await _list_managers(session_mgr)
  assert [m.id for m in managers] == [first["id"]]


@pytest.mark.asyncio
async def test_manager_endpoint_resends_orientation_when_idle_and_patrol_dead(
    tmp_path: Path,
    orientation_sends: list,
) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "charliebot-home")
  session_mgr = SessionManager(cfg)
  trigger_mgr = TriggerManager(cfg, session_mgr)
  session = await _create_manager(session_mgr)
  # A pending trigger that lacks the [patrol] prefix does not arm the chain.
  _write_trigger(
      cfg,
      PendingTrigger(
          id="unrelated-reminder",
          session_id=session.id,
          fire_at=datetime.now(timezone.utc) + timedelta(minutes=30),
          message="unrelated reminder",
      ),
  )

  result = await sessions_api.get_manager_session(session_mgr=session_mgr, trigger_mgr=trigger_mgr, cfg=cfg)

  assert result == {"id": session.id, "created": False, "patrol_armed": False}
  assert len(orientation_sends) == 1
  assert orientation_sends[0]["session_id"] == session.id
  assert orientation_sends[0]["content"] == sessions_api.MANAGER_ORIENTATION_MESSAGE


@pytest.mark.asyncio
async def test_manager_endpoint_sends_nothing_when_busy_and_patrol_dead(
    tmp_path: Path,
    orientation_sends: list,
) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "charliebot-home")
  session_mgr = SessionManager(cfg)
  trigger_mgr = TriggerManager(cfg, session_mgr)
  session = await _create_manager(session_mgr)

  thinking_state.mark_busy(session.id)
  try:
    result = await sessions_api.get_manager_session(session_mgr=session_mgr, trigger_mgr=trigger_mgr, cfg=cfg)
  finally:
    thinking_state.clear_busy(session.id)

  assert result == {"id": session.id, "created": False, "patrol_armed": False}
  assert orientation_sends == []


def test_manager_route_registered_before_session_id_route(tmp_path: Path, orientation_sends: list) -> None:
  """GET /api/sessions/manager hits the literal route, not /{session_id}.

  Matched against /{session_id}, the literal "manager" would 404 ("manager" is
  no session id); a 200 with the resolve payload proves registration order.
  """
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "charliebot-home")
  session_mgr = SessionManager(cfg)
  trigger_mgr = TriggerManager(cfg, session_mgr)
  app = FastAPI()
  app.include_router(sessions_api.router, prefix="/api/sessions")
  app.dependency_overrides[get_config] = lambda: cfg
  app.dependency_overrides[get_session_manager] = lambda: session_mgr
  app.dependency_overrides[get_trigger_manager] = lambda: trigger_mgr

  with TestClient(app) as client:
    resp = client.get("/api/sessions/manager")

  assert resp.status_code == 200
  body = resp.json()
  assert set(body.keys()) == {"id", "created", "patrol_armed"}
  assert body["created"] is True


@pytest.mark.asyncio
async def test_session_metadata_role_round_trip(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "charliebot-home")
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="Legacy"))
  metadata_path = cfg.sessions_dir / session.id / "metadata.json"

  # Old metadata files predate the role field and must load unchanged.
  raw = json.loads(metadata_path.read_text(encoding="utf-8"))
  raw.pop("role", None)
  metadata_path.write_text(json.dumps(raw, indent=2), encoding="utf-8")
  session_mgr._invalidate_cache(session.id)

  meta = await session_mgr.get_session(session.id)
  assert meta is not None
  assert meta.role is None

  # A metadata file written with role="manager" reads back the same value.
  meta.role = "manager"
  await session_mgr.save_metadata(meta)
  session_mgr._invalidate_cache(session.id)

  reread = await session_mgr.get_session(session.id)
  assert reread is not None
  assert reread.role == "manager"
