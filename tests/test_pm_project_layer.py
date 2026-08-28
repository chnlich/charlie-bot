"""Tests for the B2 project work unit: cron ``mode: master`` Project Managers.

The contract: a master-mode cron task binds one dedicated session carrying
``role=project`` and ``group=<project>``; each fire wakes that session's master
with the task's resolved prompt plus an appended ``Group:`` line (no worker
thread, no TASK_DELEGATED event); a role=project session with a group also
carries an ambient PM identity part in its master instructions; at most one
master-mode task per project; the task yaml is the single control point for
the bound session's backend and wake text.
"""

from collections.abc import Coroutine
from pathlib import Path
from types import SimpleNamespace
from typing import Any

import pytest
import yaml
from conftest import (
  SCHEDULER_CREATE_LOGGED_TASK_PATCH_TARGET,
  build_scheduler_cfg,
  close_create_logged_task,
  make_cron_client,
)
from fastapi import FastAPI
from fastapi.testclient import TestClient
from pydantic import ValidationError

from src.agents import master_cc
from src.api import sessions as sessions_api
from src.api.deps import get_session_manager
from src.core.config import (
  CharlieBotConfig,
  ImprovementLoopConfig,
  ScheduledTaskConfig,
  _validate_cron_body,
  get_config,
)
from src.core.models import (
  PROJECT_ROLE,
  CreateSessionRequest,
  SessionStatus,
)
from src.core.scheduler import Scheduler
from src.core.sessions import SessionManager

# The task's resolved prompt — the body of prompts/project_manager.md, which
# the host cron file names under prompt_file: the pointed file owns the body
# and the loader reads it on every load. The wake message appends the task's
# Group line.
PM_TASK_PROMPT = "# Project Manager\n\nDo the PM things."
PM_WAKE_PROMPT = f"{PM_TASK_PROMPT}\n\nGroup: bp-eval"


def _master_task(**overrides: Any) -> ScheduledTaskConfig:
  kwargs: dict[str, Any] = {
      "name": "pm_bp_eval",
      "cron": "30 8 * * *",
      "prompt": PM_TASK_PROMPT,
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


def test_task_config_master_mode_with_handler_is_rejected() -> None:
  with pytest.raises(ValidationError, match="mode 'master' requires a prompt source"):
    ScheduledTaskConfig(
        name="pm_x", cron="30 8 * * *", handler="backup", mode="master", project="bp-eval")


def test_task_config_master_mode_with_loop_is_rejected() -> None:
  with pytest.raises(ValidationError, match="mode 'master' requires a prompt source"):
    ScheduledTaskConfig(
        name="pm_x",
        cron="30 8 * * *",
        loop=ImprovementLoopConfig(backlog="backlog/backlog.yaml", role="reviewer", scope_files=["src/"]),
        mode="master",
        project="bp-eval")


def test_master_task_with_prompt_file_loads(tmp_path: Path) -> None:
  """prompt_file resolves into prompt before model validation, so mode master accepts it."""
  md_path = tmp_path / "pm_contract.md"
  md_path.write_text("# Project Manager\n\nThe contract.\n", encoding="utf-8")
  cfg = build_scheduler_cfg(tmp_path)
  task, _ = _validate_cron_body(
      {
          "cron": "30 8 * * *",
          "prompt_file": str(md_path),
          "mode": "master",
          "project": "bp-eval",
      },
      cfg.charlie_bot_repo,
      "pm_bp_eval")
  assert task.mode == "master"
  assert task.project == "bp-eval"
  assert task.prompt == "# Project Manager\n\nThe contract.\n"
  # the raw pointer is preserved on the runtime model
  assert task.prompt_file == str(md_path)


# ---------------------------------------------------------------------------
# Master fire: _execute_task(mode=master) wakes the session's master directly
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_master_task_fire_wakes_master_with_prompt_plus_group_line(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = build_scheduler_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  scheduler = Scheduler(cfg, session_mgr)
  task_cfg = _master_task()

  triggered: list[tuple[Any, ...]] = []
  task_names: list[str] = []
  spawned: list[Any] = []

  def fake_trigger_master(*args: Any, **kwargs: Any) -> Coroutine[Any, Any, None]:
    triggered.append(args)
    return _noop()

  def fake_create_logged_task(coro: Coroutine[Any, Any, None], name: str | None = None) -> None:
    task_names.append(name or "")
    coro.close()

  monkeypatch.setattr("src.core.scheduler.load_config", lambda: cfg)
  monkeypatch.setattr("src.core.scheduler.trigger_master", fake_trigger_master)
  monkeypatch.setattr(SCHEDULER_CREATE_LOGGED_TASK_PATCH_TARGET, fake_create_logged_task)
  monkeypatch.setattr("src.core.scheduler.spawn_worker", lambda **kwargs: spawned.append(kwargs) or _noop())

  result = await scheduler._execute_task(task_cfg)

  # No worker spawn on the master path.
  assert not spawned
  # The wake is trigger_master(session, prompt + Group line, cfg, session_mgr),
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
  cfg = build_scheduler_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  scheduler = Scheduler(cfg, session_mgr)
  task_cfg = _master_task()

  monkeypatch.setattr("src.core.scheduler.load_config", lambda: cfg)
  monkeypatch.setattr(SCHEDULER_CREATE_LOGGED_TASK_PATCH_TARGET, close_create_logged_task)
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
  cfg = build_scheduler_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  scheduler = Scheduler(cfg, session_mgr)

  monkeypatch.setattr("src.core.scheduler.load_config", lambda: cfg)
  monkeypatch.setattr(SCHEDULER_CREATE_LOGGED_TASK_PATCH_TARGET, close_create_logged_task)
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


def _master_task_payload(name: str, project: str = "bp-eval", **overrides: Any) -> dict[str, Any]:
  payload: dict[str, Any] = {
      "name": name,
      "cron": "30 8 * * *",
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
  cfg = build_scheduler_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  with make_cron_client(cfg, session_mgr) as client:
    payload = _master_task_payload("pm_orphan")
    payload.pop("project")
    resp = client.post("/api/cron/tasks", json=payload)
  assert resp.status_code == 400
  assert "requires 'project'" in resp.json()["detail"]
  assert not (cron_dir / "pm_orphan.yaml").exists()


def test_cron_create_master_task_with_prompt_file_persists_pointer(
    cron_dir: Path,
    tmp_path: Path,
) -> None:
  md_path = tmp_path / "pm_contract.md"
  md_path.write_text(PM_TASK_PROMPT + "\n", encoding="utf-8")
  cfg = build_scheduler_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  with make_cron_client(cfg, session_mgr) as client:
    payload = _master_task_payload("pm_bp_eval", prompt_file=str(md_path))
    resp = client.post("/api/cron/tasks", json=payload)

  assert resp.status_code == 200
  on_disk = yaml.safe_load((cron_dir / "pm_bp_eval.yaml").read_text(encoding="utf-8"))
  assert on_disk["prompt_file"] == str(md_path)
  assert "prompt" not in on_disk


def test_cron_create_master_task_with_unreadable_prompt_file_is_409(
    cron_dir: Path,
    tmp_path: Path,
) -> None:
  missing = tmp_path / "no_such_contract.md"
  cfg = build_scheduler_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  with make_cron_client(cfg, session_mgr) as client:
    payload = _master_task_payload("pm_bp_eval", prompt_file=str(missing))
    resp = client.post("/api/cron/tasks", json=payload)

  assert resp.status_code == 409
  assert str(missing) in resp.json()["detail"]
  assert not (cron_dir / "pm_bp_eval.yaml").exists()


def test_cron_create_second_master_task_for_project_is_409(cron_dir: Path, tmp_path: Path) -> None:
  cfg = build_scheduler_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  prompt_path = tmp_path / "pm_contract.md"
  prompt_path.write_text(PM_TASK_PROMPT + "\n", encoding="utf-8")
  with make_cron_client(cfg, session_mgr) as client:
    first = client.post(
        "/api/cron/tasks", json=_master_task_payload("pm_bp_eval", prompt_file=str(prompt_path)))
    assert first.status_code == 200
    conflict = client.post(
        "/api/cron/tasks", json=_master_task_payload("pm_bp_eval_2", prompt_file=str(prompt_path)))
    # A worker-mode task may share the project; only master tasks are exclusive.
    worker = client.post(
        "/api/cron/tasks",
        json={
            "name": "bp_eval_nightly",
            "cron": "0 2 * * *",
            "prompt_file": str(prompt_path),
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
  prompt_path = tmp_path / "pm_contract.md"
  prompt_path.write_text(PM_TASK_PROMPT + "\n", encoding="utf-8")
  (cron_dir / "pm_a.yaml").write_text(
      yaml.safe_dump({"cron": "30 8 * * *", "prompt_file": str(prompt_path), "mode": "master", "project": "bp-eval"}),
      encoding="utf-8")
  (cron_dir / "pm_b.yaml").write_text(
      yaml.safe_dump({
          "cron": "30 9 * * *",
          "prompt_file": str(prompt_path),
          "mode": "master",
          "project": "bp-eval",
          "enabled": False,
      }),
      encoding="utf-8")
  cfg = build_scheduler_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  with make_cron_client(cfg, session_mgr) as client:
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
async def test_role_bound_scheduled_session_backend_switch_writes_through_and_rotates(
    cron_dir: Path,
    tmp_path: Path,
) -> None:
  """A PM session's backend switch writes through to the task yaml and rotates."""
  cfg = build_scheduler_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  yaml_path = cron_dir / "pm_bp_eval.yaml"
  prompt_path = tmp_path / "pm_contract.md"
  prompt_path.write_text(PM_TASK_PROMPT + "\n", encoding="utf-8")
  yaml_path.write_text(
      yaml.safe_dump({
          "cron": "30 8 * * *",
          "prompt_file": str(prompt_path),
          "mode": "master",
          "project": "bp-eval",
          "enabled": True,
      }),
      encoding="utf-8")
  pm = await session_mgr.create_session(
      CreateSessionRequest(name="PM: bp-eval", scheduled_task="pm_bp_eval", role=PROJECT_ROLE),
      backend="claude-opus-4.6",
  )
  await session_mgr.set_group(pm.id, "bp-eval")

  with _build_sessions_client(cfg, session_mgr) as client:
    resp = client.post(f"/api/sessions/{pm.id}/backend", json={"backend": "codex-o3"})

  assert resp.status_code == 200
  rotated = resp.json()
  assert rotated is not None
  # Rotation created a fresh session with the same role and group.
  assert rotated["id"] != pm.id
  assert rotated["backend"] == "codex-o3"
  assert rotated["role"] == PROJECT_ROLE
  assert rotated["group"] == "bp-eval"
  assert rotated["scheduled_task"] == "pm_bp_eval"
  # The yaml picked up the backend key.
  assert yaml.safe_load(yaml_path.read_text(encoding="utf-8"))["backend"] == "codex-o3"
  # The old dedicated session is archived; the new one is the sole active one.
  reloaded = await session_mgr.get_session(pm.id)
  assert reloaded is not None
  assert reloaded.status == SessionStatus.ARCHIVED
  active = await session_mgr.list_sessions(status=SessionStatus.ACTIVE, scheduled=True)
  assert [s.id for s in active] == [rotated["id"]]


@pytest.mark.asyncio
async def test_regular_scheduled_session_without_role_keeps_clone_fork_guard(tmp_path: Path) -> None:
  """The PM guard must not overreach: role-less scheduled sessions keep existing semantics."""
  cfg = build_scheduler_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  worker = await session_mgr.create_session(
      CreateSessionRequest(name="Scheduled: nightly", scheduled_task="nightly"),
      backend="claude-opus-4.6",
  )

  with _build_sessions_client(cfg, session_mgr) as client:
    resp = client.post(f"/api/sessions/{worker.id}/backend", json={"backend": "codex-o3"})

  assert resp.status_code == 400
  assert "Clone/fork" in resp.json()["detail"]


# ---------------------------------------------------------------------------
# Master instructions: ambient PM identity part for role=project sessions
# ---------------------------------------------------------------------------


def _instructions_cfg(tmp_path: Path) -> SimpleNamespace:
  """Minimal instruction inputs: a repo base prompt and a one-entry memory store."""
  home = tmp_path / "home"
  repo = tmp_path / "repo"
  (repo / "prompts").mkdir(parents=True)
  (repo / "prompts" / "master.md").write_text("BASE PROMPT", encoding="utf-8")
  memory_dir = home / "memory"
  (memory_dir / "entries" / "profile").mkdir(parents=True)
  (memory_dir / "topics").write_text("profile resident\n", encoding="utf-8")
  (memory_dir / "entries" / "profile" / "note.md").write_text(
      "---\nscope: user\ntopic: profile\naudience: master, worker\ntitle: Note\n---\n"
      "MEMORY BODY\n",
      encoding="utf-8")
  return SimpleNamespace(
      charlie_bot_repo=repo,
      claude_md_file=home / "MASTER_AGENT_PROMPT.md",
      memory_dir=memory_dir)


def test_pm_identity_part_appended_for_project_session_with_group(tmp_path: Path) -> None:
  cfg = _instructions_cfg(tmp_path)
  meta = SimpleNamespace(id="s1", role=PROJECT_ROLE, group="bp-eval")

  out = master_cc._build_instructions_content(meta, cfg, None)

  assert out is not None
  # Pointer semantics: identity + group + contract path, not contract clauses.
  assert out.count("# Project Manager session") == 1
  assert "This session is the Project Manager for group bp-eval." in out
  assert "prompts/project_manager.md" in out
  # Appended exactly once, after the memory block, at the very end.
  assert out.index("MEMORY BODY") < out.index("# Project Manager session")
  assert out.endswith("before acting on any message in this session, and follow it.")


def test_pm_identity_part_absent_without_role_or_group(tmp_path: Path) -> None:
  cfg = _instructions_cfg(tmp_path)
  metas = [
      SimpleNamespace(id="s1", role=None, group="bp-eval"),  # group but no role
      SimpleNamespace(id="s2", role=PROJECT_ROLE, group=None),  # role but no group
      SimpleNamespace(id="s3", role=PROJECT_ROLE, group=""),  # empty group
  ]

  for meta in metas:
    out = master_cc._build_instructions_content(meta, cfg, None)
    assert out is not None
    assert "# Project Manager session" not in out
