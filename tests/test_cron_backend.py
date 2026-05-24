"""Tests for scheduled task backend overrides."""

from collections.abc import Coroutine
from pathlib import Path
from typing import Any, Optional
from unittest.mock import AsyncMock

import pytest
import yaml
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.api import cron as cron_api
from src.core.config import CharlieBotConfig, ScheduledTaskConfig
from src.core.models import BackendOption, SessionMetadata, SpawnRequest, ThreadMetadata
from src.core.scheduler import Scheduler


def _build_cfg(tmp_path: Path) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=tmp_path / "charliebot-home",
      worktree_dir=str(tmp_path / "worktrees"),
      backend_options=[
          BackendOption(id="claude-opus-4.6", label="Opus", type="cc-claude", model="claude-opus-4-6"),
          BackendOption(id="codex-o3", label="Codex", type="codex", model="o3"),
      ],
  )


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
  scheduler = Scheduler(cfg)
  session = SessionMetadata(id="session-1", name="Scheduled: nightly", backend="claude-opus-4.6")
  task_cfg = ScheduledTaskConfig(
      name="nightly",
      cron="* * * * *",
      prompt="nightly prompt",
      backend="codex-o3",
  )
  session_mgr = AsyncMock()
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
async def test_scheduler_inherits_session_backend_when_task_backend_unset(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = _build_cfg(tmp_path)
  scheduler = Scheduler(cfg)
  session = SessionMetadata(id="session-1", name="Scheduled: nightly", backend="claude-opus-4.6")
  task_cfg = ScheduledTaskConfig(name="nightly", cron="* * * * *", prompt="nightly prompt")
  session_mgr = AsyncMock()
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

  resolve_backend.assert_awaited_once_with("session-1", cfg, session_mgr, requested_backend=None)


def test_cron_api_persists_and_clears_backend(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cron_path = tmp_path / "cron.yaml"
  cron_path.parent.mkdir(parents=True, exist_ok=True)
  monkeypatch.setattr(cron_api, "CRON_PATH", cron_path)
  app = FastAPI()
  app.include_router(cron_api.router, prefix="/api/cron")

  with TestClient(app) as client:
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
