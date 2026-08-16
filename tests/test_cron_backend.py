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
from src.core.config import (
  CharlieBotConfig,
  ScheduledTaskConfig,
  _load_cron_file,
  get_config,
)
from src.core.models import (
  BackendOption,
  CreateSessionRequest,
  SessionMetadata,
  SessionStatus,
  SpawnRequest,
  ThreadMetadata,
)
from src.core.scheduler import Scheduler
from src.core.sessions import SessionManager
from src.core.thinking_state import clear_busy, mark_busy
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
  await session_mgr.save_metadata(old_session)
  # A busy session blocks backend rotation. Mark it busy in thinking_state and
  # re-fetch so the stamped thinking_since flows through the session cache.
  mark_busy(old_session.id)
  try:
    old_session_refetched = await session_mgr.get_session(old_session.id)
    assert old_session_refetched is not None
    task_cfg = ScheduledTaskConfig(
        name="nightly",
        cron="* * * * *",
        prompt="nightly prompt",
        backend="codex-o3",
    )
    execute_task = AsyncMock()
    monkeypatch.setattr(scheduler, "_execute_task", execute_task)

    await scheduler._maybe_run(task_cfg, session_mgr, {"nightly": [old_session_refetched]}, cfg)

    execute_task.assert_not_awaited()
    active_sessions = await session_mgr.list_sessions(status=SessionStatus.ACTIVE, scheduled=True)
    assert len(active_sessions) == 1
    assert active_sessions[0].id == old_session.id
    assert active_sessions[0].backend == "claude-opus-4.6"
  finally:
    clear_busy(old_session.id)


def test_cron_api_persists_and_clears_backend(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cron_dir = tmp_path / "cron.d"
  cron_dir.mkdir(parents=True, exist_ok=True)
  monkeypatch.setattr(cron_api, "cron_dir", lambda: cron_dir)
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  nightly_path = cron_dir / "nightly.yaml"

  def _read_backend() -> str:
    return yaml.safe_load(nightly_path.read_text(encoding="utf-8")).get("backend")

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
    assert _read_backend() == "codex-o3"

    clear_response = client.put("/api/cron/tasks/nightly", json={"backend": None})
    assert clear_response.status_code == 200
    assert "backend" not in clear_response.json()
    assert "backend" not in yaml.safe_load(nightly_path.read_text(encoding="utf-8"))

    update_response = client.put("/api/cron/tasks/nightly", json={"backend": "codex-o3"})
    assert update_response.status_code == 200
    assert update_response.json()["backend"] == "codex-o3"

    empty_clear_response = client.put("/api/cron/tasks/nightly", json={"backend": ""})
    assert empty_clear_response.status_code == 200
    assert "backend" not in yaml.safe_load(nightly_path.read_text(encoding="utf-8"))


def test_cron_api_rejects_invalid_backend_on_create(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cron_dir = tmp_path / "cron.d"
  cron_dir.mkdir(parents=True, exist_ok=True)
  monkeypatch.setattr(cron_api, "cron_dir", lambda: cron_dir)
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
  assert not (cron_dir / "nightly.yaml").exists()


def test_cron_api_rejects_invalid_backend_on_update(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cron_dir = tmp_path / "cron.d"
  cron_dir.mkdir(parents=True, exist_ok=True)
  (cron_dir / "nightly.yaml").write_text(
      yaml.safe_dump({"cron": "0 2 * * *", "prompt": "run nightly", "backend": "codex-o3"}),
      encoding="utf-8")
  monkeypatch.setattr(cron_api, "cron_dir", lambda: cron_dir)
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)

  with _build_cron_client(cfg, session_mgr) as client:
    response = client.put("/api/cron/tasks/nightly", json={"backend": "missing-backend"})

  assert response.status_code == 400
  assert response.json()["detail"] == "backend 'missing-backend' is not in backend_options"
  assert (
      yaml.safe_load((cron_dir / "nightly.yaml").read_text(encoding="utf-8")).get("backend")
      == "codex-o3")


@pytest.mark.asyncio
async def test_cron_api_rejects_backend_update_when_current_session_is_busy(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cron_dir = tmp_path / "cron.d"
  cron_dir.mkdir(parents=True, exist_ok=True)
  (cron_dir / "nightly.yaml").write_text(
      yaml.safe_dump({"cron": "0 2 * * *", "prompt": "run nightly", "backend": "claude-opus-4.6"}),
      encoding="utf-8")
  monkeypatch.setattr(cron_api, "cron_dir", lambda: cron_dir)
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(
      CreateSessionRequest(name="Scheduled: nightly", scheduled_task="nightly"),
      backend="claude-opus-4.6",
  )
  # A busy session blocks the backend switch with 409.
  mark_busy(session.id)
  try:
    with _build_cron_client(cfg, session_mgr) as client:
      response = client.put("/api/cron/tasks/nightly", json={"backend": "codex-o3"})

    assert response.status_code == 409
    assert "backend switch" in response.json()["detail"]
    assert (
        yaml.safe_load((cron_dir / "nightly.yaml").read_text(encoding="utf-8")).get("backend")
        == "claude-opus-4.6")
  finally:
    clear_busy(session.id)


def _seed_prompt_file_task(cron_dir: Path, tmp_path: Path) -> tuple[Path, Path, str]:
  """Write a prompt_file-backed 'nightly' job; returns (yaml_path, md_path, md_content)."""
  md_path = tmp_path / "prompts" / "nightly.md"
  md_path.parent.mkdir(parents=True, exist_ok=True)
  md_content = "Rebase omni main and report status.\n"
  md_path.write_text(md_content, encoding="utf-8")
  yaml_path = cron_dir / "nightly.yaml"
  yaml_path.write_text(
      yaml.safe_dump(
          {
              "cron": "0 3 * * *",
              "prompt_file": str(md_path),  # absolute path, as production files use
              "timezone": "America/Los_Angeles",
              "enabled": True,
          },
          sort_keys=False),
      encoding="utf-8")
  return yaml_path, md_path, md_content


def test_cron_api_updates_backend_on_prompt_file_task(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cron_dir = tmp_path / "cron.d"
  cron_dir.mkdir(parents=True, exist_ok=True)
  monkeypatch.setattr(cron_api, "cron_dir", lambda: cron_dir)
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  yaml_path, _md_path, _md_content = _seed_prompt_file_task(cron_dir, tmp_path)
  original = yaml.safe_load(yaml_path.read_text(encoding="utf-8"))

  with _build_cron_client(cfg, session_mgr) as client:
    response = client.put("/api/cron/tasks/nightly", json={"backend": "codex-o3"})

  assert response.status_code == 200
  persisted = yaml.safe_load(yaml_path.read_text(encoding="utf-8"))
  # The PUT must add exactly one key/value pair and touch nothing else, in
  # either direction.
  assert set(persisted.items()) - set(original.items()) == {("backend", "codex-o3")}
  assert set(original.items()) - set(persisted.items()) == set()

  # The persisted file must be loadable by the exact same code the production
  # loader uses — not just "parseable by this route".
  loaded, _ = _load_cron_file(yaml_path, cfg.charlie_bot_repo, "nightly")
  assert loaded.backend == "codex-o3"


def test_cron_api_rejects_prompt_edit_on_prompt_file_task(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cron_dir = tmp_path / "cron.d"
  cron_dir.mkdir(parents=True, exist_ok=True)
  monkeypatch.setattr(cron_api, "cron_dir", lambda: cron_dir)
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  yaml_path, md_path, _md_content = _seed_prompt_file_task(cron_dir, tmp_path)
  before = yaml_path.read_bytes()

  with _build_cron_client(cfg, session_mgr) as client:
    response = client.put(
        "/api/cron/tasks/nightly",
        json={"prompt": "a completely different prompt", "backend": "codex-o3"},
    )

  assert response.status_code == 400
  assert str(md_path) in response.json()["detail"]
  # The guard must fire before any write — the file on disk is untouched.
  assert yaml_path.read_bytes() == before


def test_cron_api_prompt_echo_is_not_persisted_on_prompt_file_task(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cron_dir = tmp_path / "cron.d"
  cron_dir.mkdir(parents=True, exist_ok=True)
  monkeypatch.setattr(cron_api, "cron_dir", lambda: cron_dir)
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  yaml_path, _md_path, md_content = _seed_prompt_file_task(cron_dir, tmp_path)
  original = yaml.safe_load(yaml_path.read_text(encoding="utf-8"))

  with _build_cron_client(cfg, session_mgr) as client:
    # The modal always round-trips the resolved prompt text verbatim; since it
    # matches the prompt_file content this is a no-op echo, not an edit.
    response = client.put(
        "/api/cron/tasks/nightly",
        json={
            "cron": "0 3 * * *",
            "prompt": md_content,
            "repo": None,
            "project": None,
            "timezone": "America/Los_Angeles",
            "enabled": True,
        },
    )

  assert response.status_code == 200
  persisted = yaml.safe_load(yaml_path.read_text(encoding="utf-8"))
  assert persisted == original

  loaded, _ = _load_cron_file(yaml_path, cfg.charlie_bot_repo, "nightly")
  assert loaded.prompt == md_content


def test_cron_api_tolerates_trimmed_prompt_echo_on_prompt_file_task(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cron_dir = tmp_path / "cron.d"
  cron_dir.mkdir(parents=True, exist_ok=True)
  monkeypatch.setattr(cron_api, "cron_dir", lambda: cron_dir)
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  yaml_path, _md_path, md_content = _seed_prompt_file_task(cron_dir, tmp_path)
  original = yaml.safe_load(yaml_path.read_text(encoding="utf-8"))

  with _build_cron_client(cfg, session_mgr) as client:
    # The editor's saveCronTask sends the full field set and trims the prompt
    # before the PUT, so the echo arrives without the file's trailing newline;
    # the guard must treat it as unchanged and persist the backend change.
    response = client.put(
        "/api/cron/tasks/nightly",
        json={
            "cron": "0 3 * * *",
            "prompt": md_content.strip(),
            "repo": None,
            "backend": "codex-o3",
            "project": None,
            "timezone": "America/Los_Angeles",
            "enabled": True,
        },
    )

  assert response.status_code == 200
  persisted = yaml.safe_load(yaml_path.read_text(encoding="utf-8"))
  # The PUT must add exactly the backend key/value and touch nothing else, in
  # either direction.
  assert set(persisted.items()) - set(original.items()) == {("backend", "codex-o3")}
  assert set(original.items()) - set(persisted.items()) == set()

  loaded, _ = _load_cron_file(yaml_path, cfg.charlie_bot_repo, "nightly")
  assert loaded.backend == "codex-o3"
