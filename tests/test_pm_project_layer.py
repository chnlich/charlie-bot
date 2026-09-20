"""Tests for the project work unit: cron ``type: pm`` Project Managers.

The contract: a pm-type cron task binds one dedicated session carrying
``role=project`` and ``group=<project>``; each fire runs a project liveness
check first — a group whose member sessions are all archived disables the task
(yaml ``enabled: false``), archives the dedicated session, and never wakes the
master — otherwise the fire wakes that session's master with the task's
resolved prompt plus an appended ``Group:`` line (no worker thread, no
TASK_DELEGATED event); a manually archived PM session (no elone successor) is
an intentional stop the tick turns into ``enabled: false``; a role=project
session with a group also carries an ambient PM identity part in its master
instructions; at most one pm task per project; the task yaml is the single
control point for the bound session's backend and wake text.
"""

from datetime import UTC, datetime, timedelta
from pathlib import Path
from types import SimpleNamespace
from typing import Any
from unittest.mock import AsyncMock

import pytest
import yaml
from conftest import (
    OPUS_BACKEND_ID,
    SCHEDULER_CREATE_LOGGED_TASK_PATCH_TARGET,
    SCHEDULER_GET_CONFIG_PATCH_TARGET,
    SCHEDULER_GET_SCHEDULED_TASKS_PATCH_TARGET,
    SCHEDULER_SPAWN_WORKER_PATCH_TARGET,
    SCHEDULER_TRIGGER_MASTER_PATCH_TARGET,
    _noop,
    append_events,
    build_scheduler_cfg,
    close_create_logged_task,
    make_cron_client,
    make_instruction_cfg,
    make_scheduler_setup,
    make_sessions_client,
    record_create_logged_task,
    user_event,
    write_memory_entry,
    write_memory_topics,
)
from pydantic import ValidationError

from src.agents import master_cc
from src.core.config import (
    CharlieBotConfig,
    ImprovementLoopConfig,
    ScheduledTaskConfig,
    ScheduledTaskError,
    _validate_cron_body,
    get_scheduled_task_errors,
    get_scheduled_tasks,
)
from src.core.models import (
    PROJECT_ROLE,
    CreateSessionRequest,
    LastRunStatus,
    SessionMetadata,
    SessionStatus,
)
from src.core.scheduler import pm_manually_archived

# The task's resolved prompt — the body of prompts/project_manager.md, which
# the host cron file names under prompt_file: the pointed file owns the body
# and the loader reads it on every load. The wake message appends the task's
# Group line.
PM_TASK_PROMPT = "# Project Manager\n\nDo the PM things."
PM_WAKE_PROMPT = f"{PM_TASK_PROMPT}\n\nGroup: bp-eval"


def _pm_task(**overrides: Any) -> ScheduledTaskConfig:
  kwargs: dict[str, Any] = {
      "name": "pm_bp_eval",
      "cron": "30 8 * * *",
      "prompt": PM_TASK_PROMPT,
      "type": "pm",
      "project": "bp-eval",
  }
  kwargs.update(overrides)
  return ScheduledTaskConfig(**kwargs)


# ---------------------------------------------------------------------------
# Config: the type field + validator
# ---------------------------------------------------------------------------


def test_task_config_type_is_required() -> None:
  with pytest.raises(ValidationError, match="type"):
    ScheduledTaskConfig(name="nightly", cron="0 2 * * *", prompt="run")


def test_task_config_unknown_type_is_rejected() -> None:
  with pytest.raises(ValidationError, match="Input should be 'pm' or 'normal'"):
    ScheduledTaskConfig(name="nightly", cron="0 2 * * *", prompt="run", type="boss")  # type: ignore[arg-type]


def test_task_config_pm_requires_project() -> None:
  with pytest.raises(ValidationError, match="type 'pm' requires 'project'"):
    ScheduledTaskConfig(name="pm_x", cron="30 8 * * *", prompt="wake", type="pm")


def test_task_config_pm_requires_prompt_source() -> None:
  with pytest.raises(ValidationError, match="type 'pm' requires a prompt source"):
    ScheduledTaskConfig(name="pm_x", cron="30 8 * * *", type="pm", project="bp-eval")


def test_task_config_pm_forbids_handler() -> None:
  with pytest.raises(ValidationError, match="type 'pm' forbids 'steps', 'handler', and 'loop'"):
    ScheduledTaskConfig(name="pm_x", cron="30 8 * * *", handler="backup", type="pm", project="bp-eval")


def test_task_config_pm_forbids_loop() -> None:
  with pytest.raises(ValidationError, match="type 'pm' forbids 'steps', 'handler', and 'loop'"):
    ScheduledTaskConfig(
        name="pm_x",
        cron="30 8 * * *",
        loop=ImprovementLoopConfig(backlog="backlog/backlog.yaml", role="reviewer", scope_files=["src/"]),
        type="pm",
        project="bp-eval")


def test_task_config_pm_forbids_steps() -> None:
  with pytest.raises(ValidationError, match="type 'pm' forbids 'steps', 'handler', and 'loop'"):
    ScheduledTaskConfig(
        name="pm_x", cron="30 8 * * *", type="pm", project="bp-eval", steps=[{
            "name": "one",
            "prompt": "step body",
        }])


def test_task_config_normal_requires_prompt_source() -> None:
  with pytest.raises(ValidationError, match="exactly one of"):
    ScheduledTaskConfig(name="nightly", cron="0 2 * * *", type="normal")


def test_task_config_normal_allows_no_project() -> None:
  task = ScheduledTaskConfig(name="nightly", cron="0 2 * * *", prompt="run", type="normal")
  assert task.type == "normal"
  assert task.project is None


def test_pm_task_with_prompt_file_loads(tmp_path: Path) -> None:
  """prompt_file resolves into prompt before model validation, so type pm accepts it."""
  md_path = tmp_path / "pm_contract.md"
  md_path.write_text("# Project Manager\n\nThe contract.\n", encoding="utf-8")
  cfg = build_scheduler_cfg(tmp_path)
  task, _ = _validate_cron_body(
      {
          "cron": "30 8 * * *",
          "prompt_file": str(md_path),
          "type": "pm",
          "project": "bp-eval",
      }, cfg.charlie_bot_repo, "pm_bp_eval")
  assert task.type == "pm"
  assert task.project == "bp-eval"
  assert task.prompt == "# Project Manager\n\nThe contract.\n"
  # the raw pointer is preserved on the runtime model
  assert task.prompt_file == str(md_path)


# ---------------------------------------------------------------------------
# Loader: a cron.d file without (or with an invalid) type is a loud per-file error
# ---------------------------------------------------------------------------


@pytest.fixture
def pm_home(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
  """Point CHARLIEBOT_HOME at the same dir the scheduler cfg uses, so cron_path() and
  get_scheduled_tasks() both resolve into the test tree."""
  home = tmp_path / "charliebot-home"
  monkeypatch.setenv("CHARLIEBOT_HOME", str(home))
  (home / "config.d" / "cron.d").mkdir(parents=True, exist_ok=True)
  return home


def _write_pm_yaml(
    home: Path,
    name: str = "pm_bp_eval",
    *,
    body_overrides: dict[str, Any] | None = None,
    enabled: bool = True,
) -> Path:
  """Seed one type: pm host cron file (prompt_file pointer + project), shaped like production."""
  prompt_path = home / "pm_contract.md"
  prompt_path.write_text(PM_TASK_PROMPT + "\n", encoding="utf-8")
  body: dict[str, Any] = {
      "type": "pm",
      "cron": "30 8 * * *",
      "prompt_file": str(prompt_path),
      "timezone": "America/Los_Angeles",
      "enabled": enabled,
      "project": "bp-eval",
  }
  body.update(body_overrides or {})
  path = home / "config.d" / "cron.d" / f"{name}.yaml"
  path.write_text(yaml.safe_dump(body, sort_keys=False), encoding="utf-8")
  return path


def test_cron_file_without_type_fails_load_with_scheduled_task_error(pm_home: Path) -> None:
  (pm_home / "config.d" / "cron.d" / "pm_notype.yaml").write_text(
      yaml.safe_dump({
          "cron": "30 8 * * *",
          "prompt_file": str(pm_home / "pm_contract.md"),
          "project": "bp-eval",
      }),
      encoding="utf-8")

  assert not get_scheduled_tasks()
  errors = get_scheduled_task_errors()
  assert len(errors) == 1
  assert isinstance(errors[0], ScheduledTaskError)
  assert errors[0].name == "pm_notype"
  assert "type" in errors[0].error


def test_cron_file_with_invalid_type_fails_load_with_scheduled_task_error(pm_home: Path) -> None:
  _write_pm_yaml(pm_home, "pm_broken_type", body_overrides={"type": "boss"})

  assert not get_scheduled_tasks()
  errors = get_scheduled_task_errors()
  assert len(errors) == 1
  assert isinstance(errors[0], ScheduledTaskError)
  assert errors[0].name == "pm_broken_type"
  assert "'pm'" in errors[0].error and "'normal'" in errors[0].error


# ---------------------------------------------------------------------------
# PM fire: _execute_task(type=pm) runs the liveness check, then wakes the master
# ---------------------------------------------------------------------------


async def _create_member(
    session_mgr: Any,
    *,
    name: str = "member",
    archived: bool = False,
    group: str = "bp-eval",
) -> Any:
  member = await session_mgr.create_session(CreateSessionRequest(name=name), backend=OPUS_BACKEND_ID)
  await session_mgr.set_group(member.id, group)
  if archived:
    await session_mgr.archive_session(member.id)
  return member


def _wire_pm_fire(
    monkeypatch: pytest.MonkeyPatch,
    cfg: CharlieBotConfig,
    *,
    triggered: list[Any],
    create_logged: list[str] | None = None,
    spawned: list[Any] | None = None,
) -> None:
  """Wire the scheduler module seams one PM fire crosses.

  get_config resolves to *cfg* and the master wake is recorded into *triggered*.
  The fire-and-forget task is recorded into *create_logged* when given, else closed.
  The worker spawn is recorded into *spawned* when given — the PM fire spawns no
  worker, so a site asserting that passes a list and asserts it stays empty.
  """
  monkeypatch.setattr(SCHEDULER_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  monkeypatch.setattr(SCHEDULER_TRIGGER_MASTER_PATCH_TARGET, lambda *args, **kwargs: triggered.append(args) or _noop())
  if create_logged is None:
    monkeypatch.setattr(SCHEDULER_CREATE_LOGGED_TASK_PATCH_TARGET, close_create_logged_task)
  else:
    monkeypatch.setattr(SCHEDULER_CREATE_LOGGED_TASK_PATCH_TARGET, record_create_logged_task(create_logged))
  if spawned is not None:
    monkeypatch.setattr(SCHEDULER_SPAWN_WORKER_PATCH_TARGET, lambda **kwargs: spawned.append(kwargs) or _noop())


@pytest.mark.asyncio
async def test_pm_task_fire_wakes_master_with_prompt_plus_group_line(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg, session_mgr, scheduler = make_scheduler_setup(tmp_path)
  task_cfg = _pm_task()

  triggered: list[tuple[Any, ...]] = []
  task_names: list[str] = []
  spawned: list[Any] = []
  _wire_pm_fire(monkeypatch, cfg, triggered=triggered, create_logged=task_names, spawned=spawned)

  result = await scheduler._execute_task(task_cfg)

  # No worker spawn on the PM path.
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
  assert session.last_run_status == LastRunStatus.SUCCESS


@pytest.mark.asyncio
async def test_pm_task_fire_reuses_live_session_across_fires(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg, _session_mgr, scheduler = make_scheduler_setup(tmp_path)
  task_cfg = _pm_task()

  triggered: list[tuple[Any, ...]] = []
  _wire_pm_fire(monkeypatch, cfg, triggered=triggered)

  first = await scheduler._execute_task(task_cfg)
  second = await scheduler._execute_task(task_cfg)

  assert first == second  # same dedicated session, never a second one


@pytest.mark.asyncio
async def test_pm_task_backend_rotation_carries_role_and_group(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  """Generation rotation (backend change) archives the old PM and carries role/group forward."""
  cfg, session_mgr, scheduler = make_scheduler_setup(tmp_path)

  triggered: list[tuple[Any, ...]] = []
  _wire_pm_fire(monkeypatch, cfg, triggered=triggered)

  first = await scheduler._execute_task(_pm_task())
  rotated = await scheduler._execute_task(_pm_task(backend="codex-o3"))

  assert rotated["session_id"] != first["session_id"]
  archived = await session_mgr.list_sessions(status=SessionStatus.ARCHIVED)
  assert first["session_id"] in [s.id for s in archived]
  new = await session_mgr.get_session(rotated["session_id"])
  assert new.backend == "codex-o3"
  assert new.role == PROJECT_ROLE
  assert new.group == "bp-eval"
  assert new.scheduled_task == "pm_bp_eval"


# ---------------------------------------------------------------------------
# Project liveness check: dead group disables, live group wakes
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_pm_dead_group_disables_task_archives_session_and_skips_wake(
    pm_home: Path,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  """All member sessions archived -> yaml enabled: false, PM session archived, no master wake."""
  yaml_path = _write_pm_yaml(pm_home)
  cfg, session_mgr, scheduler = make_scheduler_setup(tmp_path)
  task_cfg = _pm_task()

  archived_member = await _create_member(session_mgr, archived=True)

  triggered: list[Any] = []
  spawned: list[Any] = []
  _wire_pm_fire(monkeypatch, cfg, triggered=triggered, spawned=spawned)

  result = await scheduler._execute_task(task_cfg)

  # The master was never woken and no worker spawned.
  assert not triggered
  assert not spawned
  # The dedicated PM session was archived with a SUCCESS last-run record.
  pm = await session_mgr.get_session(result["session_id"])
  assert pm is not None
  assert pm.scheduled_task == "pm_bp_eval"
  assert pm.role == PROJECT_ROLE
  assert pm.group == "bp-eval"
  assert pm.status == SessionStatus.ARCHIVED
  assert pm.last_run_status == LastRunStatus.SUCCESS
  assert result == {"session_id": pm.id, "thread_id": None}
  # The task yaml flipped to disabled and kept every other field.
  on_disk = yaml.safe_load(yaml_path.read_text(encoding="utf-8"))
  assert on_disk["enabled"] is False
  assert on_disk["type"] == "pm"
  assert on_disk["cron"] == "30 8 * * *"
  assert on_disk["project"] == "bp-eval"
  assert on_disk["prompt_file"] == str(pm_home / "pm_contract.md")
  # The archived member was the project signal; it is untouched.
  member = await session_mgr.get_session(archived_member.id)
  assert member is not None
  assert member.status == SessionStatus.ARCHIVED


@pytest.mark.asyncio
async def test_pm_new_project_with_zero_sessions_does_not_terminate(
    pm_home: Path,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  """A brand-new project (no sessions in the group yet) wakes the PM master instead of dying."""
  yaml_path = _write_pm_yaml(pm_home)
  cfg, session_mgr, scheduler = make_scheduler_setup(tmp_path)
  task_cfg = _pm_task()

  triggered: list[tuple[Any, ...]] = []
  _wire_pm_fire(monkeypatch, cfg, triggered=triggered)

  result = await scheduler._execute_task(task_cfg)

  assert len(triggered) == 1
  assert triggered[0][1] == PM_WAKE_PROMPT
  session = await session_mgr.get_session(result["session_id"])
  assert session is not None
  assert session.status == SessionStatus.ACTIVE
  on_disk = yaml.safe_load(yaml_path.read_text(encoding="utf-8"))
  assert on_disk["enabled"] is True


@pytest.mark.asyncio
async def test_pm_active_member_session_wakes_master(
    pm_home: Path,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  """One active member session keeps the project alive even alongside archived ones."""
  yaml_path = _write_pm_yaml(pm_home)
  cfg, session_mgr, scheduler = make_scheduler_setup(tmp_path)
  task_cfg = _pm_task()

  await _create_member(session_mgr, name="old", archived=True)
  active_member = await _create_member(session_mgr, name="live")

  triggered: list[tuple[Any, ...]] = []
  _wire_pm_fire(monkeypatch, cfg, triggered=triggered)

  result = await scheduler._execute_task(task_cfg)

  assert len(triggered) == 1
  session = await session_mgr.get_session(result["session_id"])
  assert session is not None
  assert session.status == SessionStatus.ACTIVE
  on_disk = yaml.safe_load(yaml_path.read_text(encoding="utf-8"))
  assert on_disk["enabled"] is True
  fresh_member = await session_mgr.get_session(active_member.id)
  assert fresh_member is not None
  assert fresh_member.status == SessionStatus.ACTIVE


# ---------------------------------------------------------------------------
# Tick: a manually archived PM session is an intentional stop
# ---------------------------------------------------------------------------


def _scheduled_meta(
    session_id: str,
    *,
    status: SessionStatus,
    created_at: datetime,
    successor_session_id: str | None = None,
) -> SessionMetadata:
  return SessionMetadata(
      id=session_id,
      name="Scheduled: pm_bp_eval",
      scheduled_task="pm_bp_eval",
      status=status,
      created_at=created_at,
      successor_session_id=successor_session_id,
  )


def test_pm_manually_archived_newest_generation_matrix() -> None:
  task_cfg = _pm_task()
  now = datetime(2026, 6, 1, tzinfo=UTC)
  hour_ago = now - timedelta(hours=1)
  archived = _scheduled_meta("old", status=SessionStatus.ARCHIVED, created_at=hour_ago)
  eloned = _scheduled_meta("eloned", status=SessionStatus.ARCHIVED, created_at=hour_ago, successor_session_id="child")
  active = _scheduled_meta("new", status=SessionStatus.ACTIVE, created_at=now)

  # Newest generation archived with no successor: an operator stop.
  assert pm_manually_archived(task_cfg, {"pm_bp_eval": [archived]}) is True
  # Elone takeover: the archived parent carries a successor pointer.
  assert pm_manually_archived(task_cfg, {"pm_bp_eval": [eloned]}) is False
  # Backend rotation: the archived predecessor is not the newest generation.
  assert pm_manually_archived(task_cfg, {"pm_bp_eval": [archived, active]}) is False
  # Live session and unknown task: nothing to stop.
  assert pm_manually_archived(task_cfg, {"pm_bp_eval": [active]}) is False
  assert pm_manually_archived(task_cfg, {}) is False


@pytest.mark.asyncio
async def test_tick_disables_task_on_manual_pm_archive(
    pm_home: Path,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  """A manually archived PM session (no successor) flips the yaml gate off; neither the
  get-or-create nor the fire loop resurrects a generation this tick."""
  yaml_path = _write_pm_yaml(pm_home)
  cfg, session_mgr, scheduler = make_scheduler_setup(tmp_path)
  task_cfg = _pm_task()

  pm = await session_mgr.create_session(
      CreateSessionRequest(name="Scheduled: pm_bp_eval", scheduled_task="pm_bp_eval", role=PROJECT_ROLE),
      backend=OPUS_BACKEND_ID)
  await session_mgr.set_group(pm.id, "bp-eval")
  await session_mgr.archive_session(pm.id)

  get_or_create = AsyncMock()
  maybe_run = AsyncMock()
  monkeypatch.setattr(scheduler, "_get_or_create_session", get_or_create)
  monkeypatch.setattr(scheduler, "_maybe_run", maybe_run)
  monkeypatch.setattr(SCHEDULER_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  monkeypatch.setattr(SCHEDULER_GET_SCHEDULED_TASKS_PATCH_TARGET, lambda: [task_cfg])

  await scheduler._tick()

  on_disk = yaml.safe_load(yaml_path.read_text(encoding="utf-8"))
  assert on_disk["enabled"] is False
  assert on_disk["type"] == "pm"
  get_or_create.assert_not_awaited()
  maybe_run.assert_not_awaited()


@pytest.mark.asyncio
async def test_tick_elone_successor_does_not_trigger_stop_or_dead_group_termination(
    pm_home: Path,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  """An elone takeover archives the parent WITH a successor pointer and its active child is
  the newest generation: the tick keeps the task enabled and proceeds normally."""
  yaml_path = _write_pm_yaml(pm_home)
  cfg, session_mgr, scheduler = make_scheduler_setup(tmp_path)
  task_cfg = _pm_task()

  parent = await session_mgr.create_session(
      CreateSessionRequest(name="Scheduled: pm_bp_eval", scheduled_task="pm_bp_eval", role=PROJECT_ROLE),
      backend=OPUS_BACKEND_ID)
  await session_mgr.set_group(parent.id, "bp-eval")
  append_events(session_mgr.get_chat_events_path(parent.id), [user_event("e0"), user_event("e1")])
  child = await session_mgr.elone_session(parent.id, event_index=1, backend=OPUS_BACKEND_ID)

  # The succession left an archived parent (successor pointer set) and an active child.
  fresh_parent = await session_mgr.get_session(parent.id)
  assert fresh_parent is not None
  assert fresh_parent.status == SessionStatus.ARCHIVED
  assert fresh_parent.successor_session_id == child.id

  get_or_create = AsyncMock(return_value=child)
  maybe_run = AsyncMock()
  monkeypatch.setattr(scheduler, "_get_or_create_session", get_or_create)
  monkeypatch.setattr(scheduler, "_maybe_run", maybe_run)
  monkeypatch.setattr(SCHEDULER_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  monkeypatch.setattr(SCHEDULER_GET_SCHEDULED_TASKS_PATCH_TARGET, lambda: [task_cfg])

  await scheduler._tick()

  on_disk = yaml.safe_load(yaml_path.read_text(encoding="utf-8"))
  assert on_disk["enabled"] is True
  get_or_create.assert_awaited_once()
  maybe_run.assert_awaited_once()


# ---------------------------------------------------------------------------
# Session clone & elone semantics
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_fork_of_pm_session_inherits_group_but_strips_role_and_task(tmp_path: Path) -> None:
  """A fork of the PM dedicated session is an exploratory user session: it keeps the
  project group but never inherits role: project or the scheduled_task binding."""
  _cfg, session_mgr, _ = make_scheduler_setup(tmp_path)
  pm = await session_mgr.create_session(
      CreateSessionRequest(name="Scheduled: pm_bp_eval", scheduled_task="pm_bp_eval", role=PROJECT_ROLE),
      backend=OPUS_BACKEND_ID)
  await session_mgr.set_group(pm.id, "bp-eval")

  child = await session_mgr.fork_session(pm.id)

  assert child.group == "bp-eval"
  assert child.role is None
  assert child.scheduled_task is None
  # The parent stays the sole PM.
  fresh = await session_mgr.get_session(pm.id)
  assert fresh is not None
  assert fresh.role == PROJECT_ROLE
  assert fresh.scheduled_task == "pm_bp_eval"


@pytest.mark.asyncio
async def test_elone_of_pm_session_takes_inheriting_succession(
    pm_home: Path,
    tmp_path: Path,
) -> None:
  """The elone successor carries role: project, the scheduled_task binding, the group, and
  the scheduler bookkeeping; the task yaml backend is written back, and the parent is
  archived with the successor pointer set."""
  yaml_path = _write_pm_yaml(pm_home)
  _cfg, session_mgr, _ = make_scheduler_setup(tmp_path)
  pm = await session_mgr.create_session(
      CreateSessionRequest(name="Scheduled: pm_bp_eval", scheduled_task="pm_bp_eval", role=PROJECT_ROLE),
      backend=OPUS_BACKEND_ID)
  # One save carries group + bookkeeping together: a second save of the stale
  # create-time copy would clobber the group back to None.
  pm.group = "bp-eval"
  pm.last_scheduled_run = "2026-06-01T00:00:00+00:00"
  pm.last_scheduled_cron = "30 8 * * *"
  await session_mgr.save_metadata(pm)
  append_events(session_mgr.get_chat_events_path(pm.id), [user_event("e0"), user_event("e1")])

  child = await session_mgr.elone_session(pm.id, event_index=1, backend="codex-o3")

  assert child.role == PROJECT_ROLE
  assert child.scheduled_task == "pm_bp_eval"
  assert child.group == "bp-eval"
  assert child.last_scheduled_run == "2026-06-01T00:00:00+00:00"
  # The inheriting succession wrote the child's backend into the task yaml.
  on_disk = yaml.safe_load(yaml_path.read_text(encoding="utf-8"))
  assert on_disk["backend"] == "codex-o3"
  assert on_disk["enabled"] is True
  fresh = await session_mgr.get_session(pm.id)
  assert fresh is not None
  assert fresh.status == SessionStatus.ARCHIVED
  assert fresh.successor_session_id == child.id


# ---------------------------------------------------------------------------
# Cron API: one pm task per project (create + enable paths)
# ---------------------------------------------------------------------------


def _pm_task_payload(name: str, project: str = "bp-eval", **overrides: Any) -> dict[str, Any]:
  payload: dict[str, Any] = {
      "name": name,
      "cron": "30 8 * * *",
      "type": "pm",
      "project": project,
  }
  payload.update(overrides)
  return payload


def test_cron_create_pm_task_without_project_is_400(pm_home: Path, tmp_path: Path) -> None:
  cfg, session_mgr, _ = make_scheduler_setup(tmp_path)
  with make_cron_client(cfg, session_mgr) as client:
    payload = _pm_task_payload("pm_orphan")
    payload.pop("project")
    resp = client.post("/api/cron/tasks", json=payload)
  assert resp.status_code == 400
  assert "requires 'project'" in resp.json()["detail"]
  assert not (pm_home / "config.d" / "cron.d" / "pm_orphan.yaml").exists()


def test_cron_create_task_without_type_is_422(pm_home: Path, tmp_path: Path) -> None:
  """The create body requires an explicit type: a missing one is a loud 422, never a default."""
  cfg, session_mgr, _ = make_scheduler_setup(tmp_path)
  with make_cron_client(cfg, session_mgr) as client:
    resp = client.post("/api/cron/tasks", json={"name": "nightly", "cron": "0 2 * * *", "prompt": "run"})
  assert resp.status_code == 422
  assert not (pm_home / "config.d" / "cron.d" / "nightly.yaml").exists()


def test_cron_create_pm_task_with_prompt_file_persists_pointer(
    pm_home: Path,
    tmp_path: Path,
) -> None:
  md_path = tmp_path / "pm_contract.md"
  md_path.write_text(PM_TASK_PROMPT + "\n", encoding="utf-8")
  cfg, session_mgr, _ = make_scheduler_setup(tmp_path)
  with make_cron_client(cfg, session_mgr) as client:
    payload = _pm_task_payload("pm_bp_eval", prompt_file=str(md_path))
    resp = client.post("/api/cron/tasks", json=payload)

  assert resp.status_code == 200
  on_disk = yaml.safe_load((pm_home / "config.d" / "cron.d" / "pm_bp_eval.yaml").read_text(encoding="utf-8"))
  assert on_disk["prompt_file"] == str(md_path)
  assert on_disk["type"] == "pm"
  assert "prompt" not in on_disk


def test_cron_create_pm_task_with_unreadable_prompt_file_is_409(
    pm_home: Path,
    tmp_path: Path,
) -> None:
  missing = tmp_path / "no_such_contract.md"
  cfg, session_mgr, _ = make_scheduler_setup(tmp_path)
  with make_cron_client(cfg, session_mgr) as client:
    payload = _pm_task_payload("pm_bp_eval", prompt_file=str(missing))
    resp = client.post("/api/cron/tasks", json=payload)

  assert resp.status_code == 409
  assert str(missing) in resp.json()["detail"]
  assert not (pm_home / "config.d" / "cron.d" / "pm_bp_eval.yaml").exists()


def test_cron_create_second_pm_task_for_project_is_409(pm_home: Path, tmp_path: Path) -> None:
  cfg, session_mgr, _ = make_scheduler_setup(tmp_path)
  prompt_path = tmp_path / "pm_contract.md"
  prompt_path.write_text(PM_TASK_PROMPT + "\n", encoding="utf-8")
  with make_cron_client(cfg, session_mgr) as client:
    first = client.post("/api/cron/tasks", json=_pm_task_payload("pm_bp_eval", prompt_file=str(prompt_path)))
    assert first.status_code == 200
    conflict = client.post("/api/cron/tasks", json=_pm_task_payload("pm_bp_eval_2", prompt_file=str(prompt_path)))
    # A normal task may share the project label; only pm tasks are exclusive.
    normal = client.post(
        "/api/cron/tasks",
        json={
            "name": "bp_eval_nightly",
            "cron": "0 2 * * *",
            "prompt_file": str(prompt_path),
            "type": "normal",
            "project": "bp-eval",
        })

  assert conflict.status_code == 409
  assert "pm_bp_eval" in conflict.json()["detail"]
  assert "at most one" in conflict.json()["detail"]
  assert not (pm_home / "config.d" / "cron.d" / "pm_bp_eval_2.yaml").exists()
  assert normal.status_code == 200


def test_cron_update_enabling_conflicting_pm_task_is_409(pm_home: Path, tmp_path: Path) -> None:
  """Hand-edited yamls can bypass create-time dedup; the enable path must still catch it."""
  prompt_path = tmp_path / "pm_contract.md"
  prompt_path.write_text(PM_TASK_PROMPT + "\n", encoding="utf-8")
  (pm_home / "config.d" / "cron.d" / "pm_a.yaml").write_text(
      yaml.safe_dump({
          "type": "pm",
          "cron": "30 8 * * *",
          "prompt_file": str(prompt_path),
          "project": "bp-eval"
      }),
      encoding="utf-8")
  (pm_home / "config.d" / "cron.d" / "pm_b.yaml").write_text(
      yaml.safe_dump(
          {
              "type": "pm",
              "cron": "30 9 * * *",
              "prompt_file": str(prompt_path),
              "project": "bp-eval",
              "enabled": False,
          }),
      encoding="utf-8")
  cfg, session_mgr, _ = make_scheduler_setup(tmp_path)
  with make_cron_client(cfg, session_mgr) as client:
    resp = client.put("/api/cron/tasks/pm_b", json={"enabled": True})

  assert resp.status_code == 409
  assert "pm_a" in resp.json()["detail"]
  on_disk = yaml.safe_load((pm_home / "config.d" / "cron.d" / "pm_b.yaml").read_text(encoding="utf-8"))
  assert on_disk["enabled"] is False


# ---------------------------------------------------------------------------
# Session backend switch: task yaml is the single control point for PM sessions
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_role_bound_scheduled_session_backend_switch_writes_through_and_rotates(
    pm_home: Path,
    tmp_path: Path,
) -> None:
  """A PM session's backend switch writes through to the task yaml and rotates."""
  cfg, session_mgr, _ = make_scheduler_setup(tmp_path)
  yaml_path = _write_pm_yaml(pm_home)
  pm = await session_mgr.create_session(
      CreateSessionRequest(name="PM: bp-eval", scheduled_task="pm_bp_eval", role=PROJECT_ROLE),
      backend=OPUS_BACKEND_ID,
  )
  await session_mgr.set_group(pm.id, "bp-eval")

  with make_sessions_client(cfg, session_mgr) as client:
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
  cfg, session_mgr, _ = make_scheduler_setup(tmp_path)
  worker = await session_mgr.create_session(
      CreateSessionRequest(name="Scheduled: nightly", scheduled_task="nightly"),
      backend=OPUS_BACKEND_ID,
  )

  with make_sessions_client(cfg, session_mgr) as client:
    resp = client.post(f"/api/sessions/{worker.id}/backend", json={"backend": "codex-o3"})

  assert resp.status_code == 400
  assert "Clone/fork" in resp.json()["detail"]


# ---------------------------------------------------------------------------
# Master instructions: ambient PM identity part for role=project sessions
# ---------------------------------------------------------------------------


def _instructions_cfg(tmp_path: Path) -> SimpleNamespace:
  """Minimal instruction inputs: a repo base prompt and a one-entry memory store."""
  cfg = make_instruction_cfg(tmp_path, manager_contract=None)
  write_memory_topics(cfg.memory_dir, ["profile resident"])
  write_memory_entry(cfg.memory_dir, "profile", "note", title="Note", body="MEMORY BODY\n")
  return cfg


def test_pm_identity_part_appended_for_project_session_with_group(tmp_path: Path) -> None:
  cfg = _instructions_cfg(tmp_path)
  meta = SimpleNamespace(id="s1", role=PROJECT_ROLE, group="bp-eval")

  out = master_cc._build_instructions_content(meta, cfg, None)

  assert out is not None
  # Pointer semantics: identity + group + not-enabled marker + contract path,
  # not contract clauses.
  assert out.count("# Project Manager session") == 1
  assert "This session is the Project Manager for group bp-eval." in out
  assert "Your project is NOT enabled" in out
  assert "prompts/project_manager.md" in out
  # Appended exactly once, after the memory block, at the very end.
  assert out.index("MEMORY BODY") < out.index("# Project Manager session")
  assert out.endswith("old chat as enablement.")


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
