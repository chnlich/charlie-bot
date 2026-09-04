"""Tests for cron-task deletion archiving the task's dedicated scheduled sessions."""

from pathlib import Path

import pytest
from conftest import (
    OPUS_BACKEND_ID,
    append_events,
    cron_d_dir,
    dump_yaml,
    make_scheduler_setup,
    read_chat_events,
    write_cron_task,
)
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.api.cron import router as cron_router
from src.api.deps import get_config, get_session_manager
from src.api.sessions import router as sessions_router
from src.core.models import CreateSessionRequest, SessionStatus
from src.core.sessions import SessionManager


def make_cron_sessions_client(cfg, session_mgr: SessionManager) -> TestClient:
  """TestClient mounting the cron router plus the sessions router (the scheduled listing and
  unarchive endpoints) with cfg/session_mgr as dependency overrides."""
  app = FastAPI()
  app.include_router(cron_router, prefix="/api/cron")
  app.include_router(sessions_router, prefix="/api/sessions")
  app.dependency_overrides[get_config] = lambda: cfg
  app.dependency_overrides[get_session_manager] = lambda: session_mgr
  return TestClient(app)


def write_nightly_task(home: Path) -> Path:
  """Seed one healthy 'nightly' cron job (pointer-backed host file, as production files look)
  and return its yaml path."""
  prompt_path = home / "prompts" / "nightly.md"
  prompt_path.parent.mkdir(parents=True, exist_ok=True)
  prompt_path.write_text("nightly prompt", encoding="utf-8")
  return write_cron_task(
      home,
      "nightly",
      dump_yaml(
          {
              "cron": "0 3 * * *",
              "prompt_file": str(prompt_path),
              "timezone": "America/Los_Angeles",
              "enabled": True,
          }),
  )


async def make_scheduled_session(session_mgr: SessionManager, task_name: str):
  """One active session dedicated to task_name, created through the real manager."""
  return await session_mgr.create_session(
      CreateSessionRequest(name=f"Scheduled: {task_name}", scheduled_task=task_name), backend=OPUS_BACKEND_ID)


@pytest.mark.asyncio
async def test_delete_archives_task_session_and_drops_it_from_scheduled(tmp_path: Path, temp_home: Path) -> None:
  cfg, session_mgr, _ = make_scheduler_setup(tmp_path)
  write_nightly_task(temp_home)
  session = await make_scheduled_session(session_mgr, "nightly")

  with make_cron_sessions_client(cfg, session_mgr) as client:
    before = client.get("/api/sessions/scheduled")
    response = client.delete("/api/cron/tasks/nightly")
    scheduled = client.get("/api/sessions/scheduled")

  assert before.status_code == 200
  assert [s["id"] for s in before.json()] == [session.id]
  assert response.status_code == 200
  assert response.json() == {"ok": True, "archived_sessions": [session.id]}
  assert not (cron_d_dir(temp_home) / "nightly.yaml").exists()
  stored = await session_mgr.get_session(session.id)
  assert stored is not None
  assert stored.status == SessionStatus.ARCHIVED
  assert scheduled.status_code == 200
  assert scheduled.json() == []


@pytest.mark.asyncio
async def test_delete_task_without_sessions_returns_empty_archived_list(tmp_path: Path, temp_home: Path) -> None:
  cfg, session_mgr, _ = make_scheduler_setup(tmp_path)
  write_nightly_task(temp_home)

  with make_cron_sessions_client(cfg, session_mgr) as client:
    response = client.delete("/api/cron/tasks/nightly")

  assert response.status_code == 200
  assert response.json() == {"ok": True, "archived_sessions": []}
  assert not (cron_d_dir(temp_home) / "nightly.yaml").exists()


@pytest.mark.asyncio
async def test_delete_keeps_session_dir_and_history_and_unarchive_restores(tmp_path: Path, temp_home: Path) -> None:
  cfg, session_mgr, _ = make_scheduler_setup(tmp_path)
  write_nightly_task(temp_home)
  session = await make_scheduled_session(session_mgr, "nightly")
  events_path = session_mgr.get_chat_events_path(session.id)
  append_events(events_path, [{"type": "user", "content": "e0"}])

  with make_cron_sessions_client(cfg, session_mgr) as client:
    client.delete("/api/cron/tasks/nightly")
    restore = client.post(f"/api/sessions/{session.id}/unarchive")

  assert cfg.sessions_dir.joinpath(session.id).is_dir()
  assert read_chat_events(tmp_path / "charliebot-home", session.id) == [{"type": "user", "content": "e0"}]
  assert restore.status_code == 200
  assert restore.json()["status"] == SessionStatus.ACTIVE


def test_delete_shapes_missing_task_404_invalid_name_400(tmp_path: Path, temp_home: Path) -> None:
  cfg, session_mgr, _ = make_scheduler_setup(tmp_path)

  with make_cron_sessions_client(cfg, session_mgr) as client:
    missing = client.delete("/api/cron/tasks/nightly")
    invalid = client.delete("/api/cron/tasks/.lead")

  assert missing.status_code == 404
  assert missing.json() == {"detail": 'Task "nightly" not found'}
  assert invalid.status_code == 400
  assert invalid.json() == {"detail": "invalid cron name: '.lead'"}
  assert not list(cron_d_dir(temp_home).glob("*.yaml"))
