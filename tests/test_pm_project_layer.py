"""Tests for the B2 project work unit: cron ``mode: master`` Project Managers.

The contract: a master-mode cron task binds one dedicated session carrying
``role=project`` and ``group=<project>``; each fire wakes that session's master
with the exact PM inline prompt (no worker thread, no TASK_DELEGATED event);
at most one master-mode task per project; the task yaml is the single control
point for the bound session's backend.
"""

from collections.abc import Coroutine
from pathlib import Path
from typing import Any, Optional

import pytest
import yaml
from fastapi import FastAPI
from fastapi.testclient import TestClient
from pydantic import ValidationError

from src.api import cron as cron_api
from src.api import sessions as sessions_api
from src.api.deps import get_session_manager
from src.core.config import CharlieBotConfig, ScheduledTaskConfig, get_config
from src.core.models import (
  PROJECT_ROLE,
  BackendOption,
  CreateSessionRequest,
  SessionStatus,
)
from src.core.scheduler import Scheduler
from src.core.sessions import SessionManager

PM_WAKE_PROMPT = (
    "Read prompts/project_manager.md in the charlie-bot repo and run your "
    "Project Manager duties for group bp-eval.")


def _build_cfg(tmp_path: Path) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=tmp_path / "charliebot-home",
      worktree_dir=str(tmp_path / "worktrees"),
      backend_options=[
          BackendOption(id="claude-opus-4.6", label="Opus", type="cc-claude", model="claude-opus-4-6"),
          BackendOption(id="codex-o3", label="Codex", type="codex", model="o3"),
      ],
  )


def _master_task(**overrides: Any) -> ScheduledTaskConfig:
  kwargs: dict[str, Any] = {
      "name": "pm_bp_eval",
      "cron": "30 8 * * *",
      "prompt": PM_WAKE_PROMPT,
      "mode": "master",
      "project": "bp-eval",
  }
  kwargs.update(overrides)
  return ScheduledTaskConfig(**kwargs)


async def _noop() -> None:
  return None


# ---------------------------------------------------------------------------
# Config: mode field default + validator
# ---------------------------------------------------------------------------


def test_task_config_mode_defaults_to_none() -> None:
  task = ScheduledTaskConfig(name="nightly", cron="0 2 * * *", prompt="run")
  assert task.mode is None
  assert task.project is None


def test_task_config_master_mode_requires_project() -> None:
  with pytest.raises(ValidationError, match="mode 'master' requires 'project'"):
    ScheduledTaskConfig(name="pm_x", cron="30 8 * * *", prompt="wake", mode="master")


def test_task_config_worker_mode_allows_no_project() -> None:
  task = ScheduledTaskConfig(name="nightly", cron="0 2 * * *", prompt="run", mode="worker")
  assert task.mode == "worker"


# ---------------------------------------------------------------------------
# Master fire: _execute_task(mode=master) wakes the session's master inline
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_master_task_fire_wakes_master_with_exact_inline_prompt(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  scheduler = Scheduler(cfg, session_mgr)
  task_cfg = _master_task()

  triggered: list[tuple[Any, ...]] = []
  task_names: list[str] = []
  spawned: list[Any] = []

  def fake_trigger_master(*args: Any, **kwargs: Any) -> Coroutine[Any, Any, None]:
    triggered.append(args)
    return _noop()

  def fake_create_logged_task(coro: Coroutine[Any, Any, None], name: Optional[str] = None) -> None:
    task_names.append(name or "")
    coro.close()

  monkeypatch.setattr("src.core.scheduler.load_config", lambda: cfg)
  monkeypatch.setattr("src.core.scheduler.trigger_master", fake_trigger_master)
  monkeypatch.setattr("src.core.scheduler.create_logged_task", fake_create_logged_task)
  monkeypatch.setattr("src.core.scheduler.spawn_worker", lambda **kwargs: spawned.append(kwargs) or _noop())

  result = await scheduler._execute_task(task_cfg)

  # No worker spawn on the master path.
  assert spawned == []
  # The wake is trigger_master(session, exact inline prompt, cfg, session_mgr),
  # fire-and-forget through create_logged_task named by task.
  assert task_names == ["scheduled_master_pm_bp_eval"]
  assert len(triggered) == 1
  wake_session_id, wake_summary, wake_cfg, wake_session_mgr = triggered[0]
  assert wake_summary == PM_WAKE_PROMPT
  assert wake_cfg is cfg
  assert wake_session_mgr is session_mgr
  # The dedicated session exists, bound as the group's PM.
  assert result == {"session_id": wake_session_id, "thread_id": None}
  session = await session_mgr.get_session(wake_session_id)
  assert session is not None
  assert session.scheduled_task == "pm_bp_eval"
  assert session.role == PROJECT_ROLE
  assert session.group == "bp-eval"
  assert session.last_run_status == "success"


@pytest.mark.asyncio
async def test_master_task_fire_reuses_live_session_across_fires(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  scheduler = Scheduler(cfg, session_mgr)
  task_cfg = _master_task()

  monkeypatch.setattr("src.core.scheduler.load_config", lambda: cfg)
  monkeypatch.setattr("src.core.scheduler.create_logged_task", lambda coro, name=None: coro.close())
  monkeypatch.setattr("src.core.scheduler.trigger_master", lambda *args, **kwargs: _noop())

  first = await scheduler._execute_task(task_cfg)
  second = await scheduler._execute_task(task_cfg)

  assert first == second  # same dedicated session, never a second one


@pytest.mark.asyncio
async def test_master_task_backend_rotation_carries_role_and_group(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  """Generation rotation (backend change) archives the old PM and carries role/group forward."""
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  scheduler = Scheduler(cfg, session_mgr)

  monkeypatch.setattr("src.core.scheduler.load_config", lambda: cfg)
  monkeypatch.setattr("src.core.scheduler.create_logged_task", lambda coro, name=None: coro.close())
  monkeypatch.setattr("src.core.scheduler.trigger_master", lambda *args, **kwargs: _noop())

  first = await scheduler._execute_task(_master_task())
  rotated = await scheduler._execute_task(_master_task(backend="codex-o3"))

  assert rotated["session_id"] != first["session_id"]
  archived = await session_mgr.list_sessions(status=SessionStatus.ARCHIVED)
  assert first["session_id"] in [s.id for s in archived]
  new = await session_mgr.get_session(rotated["session_id"])
  assert new.backend == "codex-o3"
  assert new.role == PROJECT_ROLE
  assert new.group == "bp-eval"
  assert new.scheduled_task == "pm_bp_eval"


# ---------------------------------------------------------------------------
# Cron API: one master task per project (create + enable paths)
# ---------------------------------------------------------------------------


def _build_cron_client(cfg: CharlieBotConfig, session_mgr: SessionManager) -> TestClient:
  app = FastAPI()
  app.include_router(cron_api.router, prefix="/api/cron")
  app.dependency_overrides[get_config] = lambda: cfg
  app.dependency_overrides[get_session_manager] = lambda: session_mgr
  return TestClient(app)


def _master_task_payload(name: str, project: str = "bp-eval", **overrides: Any) -> dict[str, Any]:
  payload: dict[str, Any] = {
      "name": name,
      "cron": "30 8 * * *",
      "prompt": PM_WAKE_PROMPT,
      "mode": "master",
      "project": project,
  }
  payload.update(overrides)
  return payload


@pytest.fixture
def cron_dir(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
  # The route and get_scheduled_tasks both resolve <CHARLIEBOT_HOME>/config.d/cron.d
  # per call; point the profile home at the same dir the test cfg uses.
  home = tmp_path / "charliebot-home"
  monkeypatch.setenv("CHARLIEBOT_HOME", str(home))
  cron_dir = home / "config.d" / "cron.d"
  cron_dir.mkdir(parents=True, exist_ok=True)
  return cron_dir


def test_cron_create_master_task_without_project_is_400(cron_dir: Path, tmp_path: Path) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  with _build_cron_client(cfg, session_mgr) as client:
    payload = _master_task_payload("pm_orphan")
    payload.pop("project")
    resp = client.post("/api/cron/tasks", json=payload)
  assert resp.status_code == 400
  assert "requires 'project'" in resp.json()["detail"]
  assert not (cron_dir / "pm_orphan.yaml").exists()


def test_cron_create_second_master_task_for_project_is_409(cron_dir: Path, tmp_path: Path) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  with _build_cron_client(cfg, session_mgr) as client:
    first = client.post("/api/cron/tasks", json=_master_task_payload("pm_bp_eval"))
    assert first.status_code == 200
    conflict = client.post("/api/cron/tasks", json=_master_task_payload("pm_bp_eval_2"))
    # A worker-mode task may share the project; only master tasks are exclusive.
    worker = client.post(
        "/api/cron/tasks",
        json={
            "name": "bp_eval_nightly",
            "cron": "0 2 * * *",
            "prompt": "run nightly",
            "mode": "worker",
            "project": "bp-eval",
        })

  assert conflict.status_code == 409
  assert "pm_bp_eval" in conflict.json()["detail"]
  assert "at most one" in conflict.json()["detail"]
  assert not (cron_dir / "pm_bp_eval_2.yaml").exists()
  assert worker.status_code == 200


def test_cron_update_enabling_conflicting_master_task_is_409(cron_dir: Path, tmp_path: Path) -> None:
  """Hand-edited yamls can bypass create-time dedup; the enable path must still catch it."""
  (cron_dir / "pm_a.yaml").write_text(
      yaml.safe_dump({"cron": "30 8 * * *", "prompt": "wake", "mode": "master", "project": "bp-eval"}),
      encoding="utf-8")
  (cron_dir / "pm_b.yaml").write_text(
      yaml.safe_dump({
          "cron": "30 9 * * *",
          "prompt": "wake",
          "mode": "master",
          "project": "bp-eval",
          "enabled": False,
      }),
      encoding="utf-8")
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  with _build_cron_client(cfg, session_mgr) as client:
    resp = client.put("/api/cron/tasks/pm_b", json={"enabled": True})

  assert resp.status_code == 409
  assert "pm_a" in resp.json()["detail"]
  on_disk = yaml.safe_load((cron_dir / "pm_b.yaml").read_text(encoding="utf-8"))
  assert on_disk["enabled"] is False


# ---------------------------------------------------------------------------
# Session backend switch: task yaml is the single control point for PM sessions
# ---------------------------------------------------------------------------


def _build_sessions_client(cfg: CharlieBotConfig, session_mgr: SessionManager) -> TestClient:
  app = FastAPI()
  app.include_router(sessions_api.router, prefix="/api/sessions")
  app.dependency_overrides[get_config] = lambda: cfg
  app.dependency_overrides[get_session_manager] = lambda: session_mgr
  return TestClient(app)


@pytest.mark.asyncio
async def test_role_bound_scheduled_session_backend_switch_is_409(tmp_path: Path) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  pm = await session_mgr.create_session(
      CreateSessionRequest(name="PM: bp-eval", scheduled_task="pm_bp_eval", role=PROJECT_ROLE),
      backend="claude-opus-4.6",
  )
  await session_mgr.set_group(pm.id, "bp-eval")

  with _build_sessions_client(cfg, session_mgr) as client:
    resp = client.post(f"/api/sessions/{pm.id}/backend", json={"backend": "codex-o3"})

  assert resp.status_code == 409
  detail = resp.json()["detail"]
  assert "pm_bp_eval" in detail
  assert "config.d/cron.d/pm_bp_eval.yaml" in detail
  # The switch did not happen in place.
  reloaded = await session_mgr.get_session(pm.id)
  assert reloaded.backend == "claude-opus-4.6"


@pytest.mark.asyncio
async def test_regular_scheduled_session_without_role_keeps_clone_fork_guard(tmp_path: Path) -> None:
  """The PM guard must not overreach: role-less scheduled sessions keep existing semantics."""
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  worker = await session_mgr.create_session(
      CreateSessionRequest(name="Scheduled: nightly", scheduled_task="nightly"),
      backend="claude-opus-4.6",
  )

  with _build_sessions_client(cfg, session_mgr) as client:
    resp = client.post(f"/api/sessions/{worker.id}/backend", json={"backend": "codex-o3"})

  assert resp.status_code == 400
  assert "Clone/fork" in resp.json()["detail"]
