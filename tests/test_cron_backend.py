"""Tests for scheduled task backend overrides."""

from collections.abc import Coroutine
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any, Optional
from unittest.mock import AsyncMock
from zoneinfo import ZoneInfo

import pytest
import yaml
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.api import cron as cron_api
from src.api.deps import get_session_manager
from src.core.config import CharlieBotConfig, ScheduledTaskConfig, get_config
from src.core.models import BackendOption, CreateSessionRequest, SessionMetadata, SessionStatus, SpawnRequest
from src.core.models import ThreadMetadata
from src.core.scheduler import Scheduler
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager


def _build_cfg(tmp_path: Path) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=tmp_path / "charliebot-home",
      worktree_dir=str(tmp_path / "worktrees"),
      backend_options=[
          BackendOption(id="claude-opus-4.6", label="Opus", type="cc-claude", model="claude-opus-4-6"),
          BackendOption(id="codex-o3", label="Codex", type="codex", model="o3"),
      ],
  )


def _build_cron_client(cfg: CharlieBotConfig, session_mgr: SessionManager) -> TestClient:
  app = FastAPI()
  app.include_router(cron_api.router, prefix="/api/cron")
  app.dependency_overrides[get_config] = lambda: cfg
  app.dependency_overrides[get_session_manager] = lambda: session_mgr
  return TestClient(app)


async def _noop() -> None:
  return None


class FakeThreadManager:
  """Minimal ThreadManager double for scheduler backend tests."""

  def __init__(self) -> None:
    self.thread = ThreadMetadata(id="thread-1", session_id="session-1", description="nightly prompt")

  async def create_thread(
      self,
      session: SessionMetadata,
      description: str,
      require_review: bool = True,
  ) -> ThreadMetadata:
    self.thread.session_id = session.id
    self.thread.description = description
    self.thread.require_review = require_review
    return self.thread


@pytest.mark.asyncio
async def test_scheduler_uses_task_backend_override_for_scheduled_worker(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = AsyncMock()
  scheduler = Scheduler(cfg, session_mgr)
  session = SessionMetadata(id="session-1", name="Scheduled: nightly", backend="claude-opus-4.6")
  task_cfg = ScheduledTaskConfig(
      name="nightly",
      cron="* * * * *",
      prompt="nightly prompt",
      backend="codex-o3",
  )
  fake_thread_mgr = FakeThreadManager()
  resolve_backend = AsyncMock(return_value=("codex-o3", "o3"))
  spawn_request: Optional[SpawnRequest] = None

  def fake_spawn_worker(**kwargs: Any) -> Coroutine[Any, Any, None]:
    nonlocal spawn_request
    spawn_request = kwargs["request"]
    return _noop()

  def fake_create_logged_task(coro: Coroutine[Any, Any, None], name: Optional[str] = None) -> None:
    coro.close()

  monkeypatch.setattr("src.core.scheduler.ThreadManager", lambda _cfg: fake_thread_mgr)
  monkeypatch.setattr("src.core.scheduler.resolve_requested_subagent_backend_model", resolve_backend)
  monkeypatch.setattr("src.core.scheduler.spawn_worker", fake_spawn_worker)
  monkeypatch.setattr("src.core.scheduler.create_logged_task", fake_create_logged_task)

  result = await scheduler._spawn_scheduled_worker(
      session,
      task_cfg,
      "nightly prompt",
      "nightly prompt",
      "scheduled_task_fired",
      cfg,
      session_mgr,
      require_review=False)

  assert result == {"session_id": "session-1", "thread_id": "thread-1"}
  resolve_backend.assert_awaited_once_with("session-1", cfg, session_mgr, requested_backend="codex-o3")
  assert spawn_request is not None
  assert spawn_request.resolved_backend == "codex-o3"
  assert spawn_request.resolved_model == "o3"
  session_mgr.persist_and_broadcast.assert_awaited_once()
  event = session_mgr.persist_and_broadcast.await_args.args[1]
  assert event["backend"] == "codex-o3"
  assert event["model"] == "o3"


@pytest.mark.asyncio
async def test_scheduler_uses_default_backend_when_task_backend_unset(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = AsyncMock()
  scheduler = Scheduler(cfg, session_mgr)
  session = SessionMetadata(id="session-1", name="Scheduled: nightly", backend="claude-opus-4.6")
  task_cfg = ScheduledTaskConfig(name="nightly", cron="* * * * *", prompt="nightly prompt")
  fake_thread_mgr = FakeThreadManager()
  resolve_backend = AsyncMock(return_value=("claude-opus-4.6", "claude-opus-4-6"))

  def fake_spawn_worker(**_kwargs: Any) -> Coroutine[Any, Any, None]:
    return _noop()

  def fake_create_logged_task(coro: Coroutine[Any, Any, None], name: Optional[str] = None) -> None:
    coro.close()

  monkeypatch.setattr("src.core.scheduler.ThreadManager", lambda _cfg: fake_thread_mgr)
  monkeypatch.setattr("src.core.scheduler.resolve_requested_subagent_backend_model", resolve_backend)
  monkeypatch.setattr("src.core.scheduler.spawn_worker", fake_spawn_worker)
  monkeypatch.setattr("src.core.scheduler.create_logged_task", fake_create_logged_task)

  await scheduler._spawn_scheduled_worker(
      session,
      task_cfg,
      "nightly prompt",
      "nightly prompt",
      "scheduled_task_fired",
      cfg,
      session_mgr,
      require_review=False)

  resolve_backend.assert_awaited_once_with("session-1", cfg, session_mgr, requested_backend="claude-opus-4.6")


@pytest.mark.asyncio
async def test_scheduler_rotates_scheduled_session_backend_and_copies_bookkeeping(tmp_path: Path) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  scheduler = Scheduler(cfg, session_mgr)
  old_session = await session_mgr.create_session(
      CreateSessionRequest(name="Scheduled: nightly", scheduled_task="nightly"),
      backend="claude-opus-4.6",
  )
  old_session.last_scheduled_run = "2026-06-07T02:00:00-07:00"
  old_session.last_scheduled_cron = "0 2 * * *"
  old_session.last_run_status = "success"
  old_session.cc_session_id = "old-backend-conversation"
  old_session.cc_session_started_at = datetime(2026, 6, 7, 9, 0, tzinfo=timezone.utc)
  await session_mgr.save_metadata(old_session)
  thread_mgr = ThreadManager(cfg)
  old_thread = await thread_mgr.create_thread(old_session, "old backend thread")

  task_cfg = ScheduledTaskConfig(
      name="nightly",
      cron="0 2 * * *",
      prompt="nightly prompt",
      backend="codex-o3",
  )

  new_session = await scheduler._get_or_create_session(task_cfg, cfg, session_mgr)

  assert new_session is not None
  assert new_session.id != old_session.id
  assert new_session.backend == "codex-o3"
  assert new_session.scheduled_task == "nightly"
  assert new_session.last_scheduled_run == old_session.last_scheduled_run
  assert new_session.last_scheduled_cron == old_session.last_scheduled_cron
  assert new_session.last_run_status == old_session.last_run_status
  assert new_session.cc_session_id is None
  assert new_session.cc_session_started_at is None
  assert await thread_mgr.list_threads(new_session.id) == []
  assert [thread.id for thread in await thread_mgr.list_threads(old_session.id)] == [old_thread.id]
  archived_old = await session_mgr.get_session(old_session.id)
  assert archived_old is not None
  assert archived_old.status == SessionStatus.ARCHIVED


@pytest.mark.asyncio
async def test_scheduler_backend_rotation_preserves_last_run_to_avoid_duplicate_fire(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  scheduler = Scheduler(cfg, session_mgr)
  old_session = await session_mgr.create_session(
      CreateSessionRequest(name="Scheduled: nightly", scheduled_task="nightly"),
      backend="claude-opus-4.6",
  )
  now = datetime.now(ZoneInfo("America/Los_Angeles"))
  old_session.last_scheduled_run = now.isoformat()
  old_session.last_scheduled_cron = "* * * * *"
  old_session.last_run_status = "success"
  await session_mgr.save_metadata(old_session)
  task_cfg = ScheduledTaskConfig(
      name="nightly",
      cron="* * * * *",
      prompt="nightly prompt",
      backend="codex-o3",
  )
  execute_task = AsyncMock()
  monkeypatch.setattr(scheduler, "_execute_task", execute_task)

  await scheduler._maybe_run(task_cfg, session_mgr, {"nightly": [old_session]}, cfg)

  execute_task.assert_not_awaited()
  active_sessions = await session_mgr.list_sessions(status=SessionStatus.ACTIVE, scheduled=True)
  assert len(active_sessions) == 1
  assert active_sessions[0].backend == "codex-o3"
  assert active_sessions[0].last_scheduled_run == now.isoformat()
  assert active_sessions[0].last_scheduled_cron == "* * * * *"


@pytest.mark.asyncio
async def test_scheduler_skips_backend_rotation_while_old_session_is_running(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  scheduler = Scheduler(cfg, session_mgr)
  old_session = await session_mgr.create_session(
      CreateSessionRequest(name="Scheduled: nightly", scheduled_task="nightly"),
      backend="claude-opus-4.6",
  )
  old_session.last_scheduled_run = (datetime.now(ZoneInfo("America/Los_Angeles")) - timedelta(hours=1)).isoformat()
  old_session.last_scheduled_cron = "* * * * *"
  old_session.thinking_since = datetime.now(timezone.utc)
  await session_mgr.save_metadata(old_session)
  task_cfg = ScheduledTaskConfig(
      name="nightly",
      cron="* * * * *",
      prompt="nightly prompt",
      backend="codex-o3",
  )
  execute_task = AsyncMock()
  monkeypatch.setattr(scheduler, "_execute_task", execute_task)

  await scheduler._maybe_run(task_cfg, session_mgr, {"nightly": [old_session]}, cfg)

  execute_task.assert_not_awaited()
  active_sessions = await session_mgr.list_sessions(status=SessionStatus.ACTIVE, scheduled=True)
  assert len(active_sessions) == 1
  assert active_sessions[0].id == old_session.id
  assert active_sessions[0].backend == "claude-opus-4.6"


def test_cron_api_persists_and_clears_backend(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cron_path = tmp_path / "cron.yaml"
  cron_path.parent.mkdir(parents=True, exist_ok=True)
  monkeypatch.setattr(cron_api, "cron_path", lambda: cron_path)
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)

  with _build_cron_client(cfg, session_mgr) as client:
    create_response = client.post(
        "/api/cron/tasks",
        json={
            "name": "nightly",
            "cron": "0 2 * * *",
            "prompt": "run nightly",
            "backend": "codex-o3",
        },
    )
    assert create_response.status_code == 200
    assert create_response.json()["backend"] == "codex-o3"
    assert yaml.safe_load(cron_path.read_text(encoding="utf-8"))["scheduled_tasks"][0]["backend"] == "codex-o3"

    clear_response = client.put("/api/cron/tasks/nightly", json={"backend": None})
    assert clear_response.status_code == 200
    assert "backend" not in clear_response.json()
    assert "backend" not in yaml.safe_load(cron_path.read_text(encoding="utf-8"))["scheduled_tasks"][0]

    update_response = client.put("/api/cron/tasks/nightly", json={"backend": "codex-o3"})
    assert update_response.status_code == 200
    assert update_response.json()["backend"] == "codex-o3"

    empty_clear_response = client.put("/api/cron/tasks/nightly", json={"backend": ""})
    assert empty_clear_response.status_code == 200
    assert "backend" not in yaml.safe_load(cron_path.read_text(encoding="utf-8"))["scheduled_tasks"][0]


def test_cron_api_rejects_invalid_backend_on_create(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cron_path = tmp_path / "cron.yaml"
  monkeypatch.setattr(cron_api, "cron_path", lambda: cron_path)
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)

  with _build_cron_client(cfg, session_mgr) as client:
    response = client.post(
        "/api/cron/tasks",
        json={
            "name": "nightly",
            "cron": "0 2 * * *",
            "prompt": "run nightly",
            "backend": "missing-backend",
        },
    )

  assert response.status_code == 400
  assert response.json()["detail"] == "backend 'missing-backend' is not in backend_options"
  assert not cron_path.exists()


def test_cron_api_rejects_invalid_backend_on_update(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cron_path = tmp_path / "cron.yaml"
  initial_task = {
      "name": "nightly",
      "cron": "0 2 * * *",
      "prompt": "run nightly",
      "backend": "codex-o3",
  }
  cron_path.write_text(
      yaml.safe_dump({"scheduled_tasks": [initial_task]}),
      encoding="utf-8",
  )
  monkeypatch.setattr(cron_api, "cron_path", lambda: cron_path)
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)

  with _build_cron_client(cfg, session_mgr) as client:
    response = client.put("/api/cron/tasks/nightly", json={"backend": "missing-backend"})

  assert response.status_code == 400
  assert response.json()["detail"] == "backend 'missing-backend' is not in backend_options"
  assert yaml.safe_load(cron_path.read_text(encoding="utf-8"))["scheduled_tasks"][0]["backend"] == "codex-o3"


@pytest.mark.asyncio
async def test_cron_api_rejects_backend_update_when_current_session_is_busy(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cron_path = tmp_path / "cron.yaml"
  initial_task = {
      "name": "nightly",
      "cron": "0 2 * * *",
      "prompt": "run nightly",
      "backend": "claude-opus-4.6",
  }
  cron_path.write_text(
      yaml.safe_dump({"scheduled_tasks": [initial_task]}),
      encoding="utf-8",
  )
  monkeypatch.setattr(cron_api, "cron_path", lambda: cron_path)
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(
      CreateSessionRequest(name="Scheduled: nightly", scheduled_task="nightly"),
      backend="claude-opus-4.6",
  )
  session.thinking_since = datetime.now(timezone.utc)
  await session_mgr.save_metadata(session)

  with _build_cron_client(cfg, session_mgr) as client:
    response = client.put("/api/cron/tasks/nightly", json={"backend": "codex-o3"})

  assert response.status_code == 409
  assert "backend switch" in response.json()["detail"]
  assert yaml.safe_load(cron_path.read_text(encoding="utf-8"))["scheduled_tasks"][0]["backend"] == "claude-opus-4.6"
