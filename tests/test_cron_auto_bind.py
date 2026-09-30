"""The scheduler's auto-bind: every scheduled task binds a task-tree manager node.

The plan's section 4.2: ``session_id`` is guaranteed by the scheduler. A task
without a binding \u2014 enabled or disabled, with or without a legacy cron session
\u2014 is bound on the tick that sees it: the node is created (request id from the
task name alone), the old cron session's scheduler bookkeeping migrates onto
it, the binding is written back through the single-key write, and the old cron
sessions are archived unconditionally. The bound node then takes over the cron
session's duties: backend follows the task config, archiving it disables the
task, deleting the task leaves it untouched.
"""

from __future__ import annotations

import asyncio
import json
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any
from unittest.mock import AsyncMock

import pytest
import yaml
from conftest import (
    CODEX_BACKEND_OPTION,
    OPUS_BACKEND_ID,
    OPUS_BACKEND_OPTION,
    bind_deps_managers,
    init_repo_with_origin,
    make_cron_sessions_client,
    make_legacy_cron_session,
    patch_instructions_content,
    write_nightly_prompt,
    write_nightly_task,
)

from src.core.config import CharlieBotConfig, ScheduledTaskConfig
from src.core.models import SessionStatus, ThreadMetadata, ThreadStatus, utc_now_iso
from src.core.scheduler import TASK_HANDLERS, Scheduler
from src.core.sessions import SessionManager
from src.core.task_sessions import TaskTreeManager
from tests.test_cron_backend import _patch_cron_d
from tests.test_task_execution import (
    BUILD_BACKEND_PATCH_TARGET,
    WORKER_BUILD_BACKEND_PATCH_TARGET,
    SpawningScriptedBackend,
    _adapter_with_silent_broadcast,
    install_backends,
    result_event,
    stub_credentials,
    wait_for_terminal_run,
)

_NIGHTLY_PROMPT_MD = "run nightly\n"


def _read_task_yaml(home: Path, name: str = "nightly") -> dict:
  return yaml.safe_load((home / ".charliebot" / "config.d" / "cron.d" / f"{name}.yaml").read_text())


def _write_task_body(home: Path, name: str, body: dict) -> None:
  path = home / ".charliebot" / "config.d" / "cron.d" / f"{name}.yaml"
  path.parent.mkdir(parents=True, exist_ok=True)
  path.write_text(yaml.safe_dump(body, sort_keys=False), encoding="utf-8")


@pytest.fixture()
def tick_env(tmp_path: Path, temp_home: Path, monkeypatch: pytest.MonkeyPatch):
  """One synthetic home the scheduler tick reads: cron.d under HOME's profile,
  sessions under the cfg's own home, tree wired as the deps singleton. Yields
  (cfg, session_mgr, tree, scheduler, home)."""
  cfg = CharlieBotConfig(
      charliebot_home=tmp_path / "charliebot-home",
      backends={"options": [OPUS_BACKEND_OPTION, CODEX_BACKEND_OPTION]},
      paths={"worktree_dir": str(tmp_path / "worktrees")})
  monkeypatch.setattr("src.core.scheduler.get_config", lambda: cfg)
  cfg.sessions_dir.mkdir(parents=True, exist_ok=True)
  session_mgr = SessionManager(cfg)
  tree = TaskTreeManager(cfg, session_mgr)
  bind_deps_managers(monkeypatch, tree, session_mgr)
  scheduler = Scheduler(cfg, session_mgr)
  return cfg, session_mgr, tree, scheduler, temp_home


# ---------------------------------------------------------------------------
# The migration tick: bind, copy bookkeeping, write back, archive
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_tick_auto_binds_unbound_task_with_active_legacy_cron_session(tick_env) -> None:
  """One tick migrates an unbound enabled task with an active legacy cron
  session: the yaml gains only ``session_id``, the node is a manager named
  after the task in the task's project group carrying the copied last run, the
  cron session is archived, and nothing fires at the migration moment."""
  _cfg, session_mgr, tree, scheduler, home = tick_env
  write_nightly_task(home, project="charlie", backend=OPUS_BACKEND_ID)
  cron_session = await make_legacy_cron_session(session_mgr, "nightly")
  # A copied anchor whose next occurrence is always ahead of this tick, so the
  # migration itself fires nothing.
  copied_run = (datetime.now(UTC) + timedelta(hours=2)).isoformat()
  cron_session.last_scheduled_run = copied_run
  cron_session.last_scheduled_cron = "0 3 * * *"
  await session_mgr.save_metadata(cron_session)
  execute_task = AsyncMock()
  scheduler._execute_task = execute_task  # type: ignore[method-assign]

  await scheduler._tick()

  # The yaml changed by exactly one key.
  body = _read_task_yaml(home)
  assert isinstance(body.get("session_id"), str) and body["session_id"]
  assert body["cron"] == "0 3 * * *" and body["enabled"] is True
  assert body["project"] == "charlie" and body["backend"] == OPUS_BACKEND_ID
  assert body["prompt_file"].endswith("nightly.md")
  # The node: a manager named after the task, grouped under the project, on
  # the task's effective backend.
  node = await tree.load_meta(body["session_id"])
  assert node is not None
  assert node.profile == "manager" and node.name == "nightly" and node.group == "charlie"
  assert node.backend == OPUS_BACKEND_ID
  assert node.scheduled_task is None  # a task-tree node, never re-stamped
  # The copied bookkeeping: no catch-up fire at the migration moment.
  assert node.last_scheduled_run == copied_run
  assert node.last_scheduled_cron == "0 3 * * *"
  execute_task.assert_not_awaited()
  # The legacy cron session is archived.
  stored = await session_mgr.get_session(cron_session.id)
  assert stored is not None and stored.status == SessionStatus.ARCHIVED


@pytest.mark.asyncio
async def test_task_without_project_lands_ungrouped(tick_env) -> None:
  _cfg, _session_mgr, tree, scheduler, home = tick_env
  write_nightly_task(home)

  await scheduler._tick()

  body = _read_task_yaml(home)
  node = await tree.load_meta(body["session_id"])
  assert node is not None
  assert node.group is None


@pytest.mark.asyncio
async def test_disabled_task_and_task_without_prior_cron_session_are_bound_too(tick_env) -> None:
  """A disabled unbound task still shows as a node, and a task with no prior
  cron session binds the same way."""
  _cfg, session_mgr, tree, scheduler, home = tick_env
  prompt_path = write_nightly_prompt(home, _NIGHTLY_PROMPT_MD)
  _write_task_body(
      home, "nightly", {
          "cron": "0 3 * * *",
          "prompt_file": str(prompt_path),
          "timezone": "America/Los_Angeles",
          "enabled": False,
      })
  _write_task_body(
      home, "dawn", {
          "cron": "0 4 * * *",
          "prompt_file": str(prompt_path),
          "timezone": "America/Los_Angeles",
          "enabled": True,
      })

  await scheduler._tick()

  for name in ("nightly", "dawn"):
    body = _read_task_yaml(home, name)
    node = await tree.load_meta(body["session_id"])
    assert node is not None and node.profile == "manager" and node.name == name
  # Neither task had a cron session; none was created.
  assert not await session_mgr.list_sessions(scheduled=True)


@pytest.mark.asyncio
async def test_handler_task_binds_and_records_its_result_on_the_node(tick_env) -> None:
  """A bound handler task's fire records its handler result on the node and
  creates no child."""
  _cfg, session_mgr, tree, scheduler, home = tick_env
  _write_task_body(
      home, "nightly", {
          "cron": "0 3 * * *",
          "handler": "probe",
          "timezone": "America/Los_Angeles",
          "enabled": True,
      })

  await scheduler._tick()  # the binding tick: the handler is not yet due

  node_id = _read_task_yaml(home)["session_id"]
  node = await tree.load_meta(node_id)
  assert node is not None
  # Rewind the anchor onto a past occurrence so the next tick is due.
  await tree.record_scheduled_fire(node_id, last_scheduled_run="2026-01-01T03:00:00+00:00")
  from unittest.mock import patch
  with patch.dict(TASK_HANDLERS, {"probe": AsyncMock(return_value="swept 42 bytes")}):
    await scheduler._tick()

  fresh = await tree.load_meta(node_id)
  assert fresh is not None and fresh.last_scheduled_run is not None
  events = [e for e in session_mgr.load_chat_events_sync(node_id) if e.get("type") == "handler_result"]
  assert [e.get("message") for e in events] == ["swept 42 bytes"]
  # Handler work is inline: the node has no child.
  index = await tree._get_index()
  assert not [m for m in index.metas.values() if m.task_parent_id == node_id]


@pytest.mark.asyncio
@pytest.mark.integration  # polls the fired run's terminal fact in real time through the full dispatch pipeline
async def test_due_fire_after_migration_creates_its_leaf_under_the_node(
    tick_env, tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The same tick that binds a task evaluates its fire against the new node:
  the copied (old) last run says the task is due, and the leaf lands under the
  node \u2014 never under a cron session. The leaf spec matches the unbound
  path's on the base revision (type-less, repo-bound, no base branch, the
  task's backend), and the leaf's work run actually launches to a spawned
  process \u2014 the 2026-09-26 ``KeyError: None`` regression came from a
  type-less repo-bound leaf that no test ever launched."""
  cfg, session_mgr, tree, scheduler, home = tick_env
  repo, _origin = init_repo_with_origin(tmp_path / "sweep-work")
  write_nightly_task(home, backend=OPUS_BACKEND_ID, repo=str(repo))
  cron_session = await make_legacy_cron_session(session_mgr, "nightly")
  # Last ran at yesterday's 03:00 occurrence: today's 03:00 is due.
  cron_session.last_scheduled_run = "2026-06-07T03:00:00-07:00"
  cron_session.last_scheduled_cron = "0 3 * * *"
  await session_mgr.save_metadata(cron_session)
  stub_credentials({"charliebot": {"access_key": "op-secret"}})
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
  install_backends(
      monkeypatch, [SpawningScriptedBackend([result_event("sweep done")])], WORKER_BUILD_BACKEND_PATCH_TARGET)

  await scheduler._tick()

  node_id = _read_task_yaml(home)["session_id"]
  leaves = [m for m in (await tree._get_index()).metas.values() if m.task_parent_id == node_id]
  assert len(leaves) == 1
  leaf = leaves[0]
  assert leaf.profile == "worker"
  # The unbound path's leaf spec, verbatim (ensure_firing_leaf is shared).
  assert leaf.task is not None and leaf.task.task_type is None
  assert leaf.task.repo_path == str(repo) and leaf.task.base_branch is None
  # The work run launched: one spawned process carried it to a real terminal
  # fact on the task's effective backend.
  runs = tree.runs.list_run_records_sync(leaf.id)
  deadline = asyncio.get_event_loop().time() + 15
  while not runs and asyncio.get_event_loop().time() < deadline:
    await asyncio.sleep(0.05)
    runs = tree.runs.list_run_records_sync(leaf.id)
  assert len(runs) == 1
  run, outcome = await wait_for_terminal_run(tree, leaf.id, runs[0].id)
  assert outcome == "success"
  assert run.kind == "work"
  assert run.pid == 424001 and run.pid_start == "1-424000"
  assert run.backend == OPUS_BACKEND_ID and run.model == "claude-opus-4-6"
  # The cron session is archived and holds no firing: the work went to the node.
  stored = await session_mgr.get_session(cron_session.id)
  assert stored is not None and stored.status == SessionStatus.ARCHIVED
  index = await tree._get_index()
  assert not [m for m in index.metas.values() if m.task_parent_id == cron_session.id]


# ---------------------------------------------------------------------------
# Crash replay: each interrupted step re-runs into the same end state
# ---------------------------------------------------------------------------


def _boom(*args: Any, **kwargs: Any) -> Any:
  # Sync on purpose: the write-back runs through asyncio.to_thread, and an
  # async stand-in would return an un-awaited coroutine instead of crashing.
  raise RuntimeError("crashed")


@pytest.mark.asyncio
@pytest.mark.parametrize("fail_after", ["create", "copy", "write"])
async def test_crash_replay_after_each_step_ends_in_one_node_one_binding_no_active_cron_session(
    tick_env, monkeypatch: pytest.MonkeyPatch, fail_after: str) -> None:
  """Stopping auto-bind after each of the first three steps and running the
  tick again ends with exactly one node, one binding, and no active cron
  session for the task."""
  _cfg, session_mgr, tree, scheduler, home = tick_env
  write_nightly_task(home)
  cron_session = await make_legacy_cron_session(session_mgr, "nightly")
  # A just-ran anchor: the next occurrence is always ahead, so no replayed tick
  # fires and the copied bookkeeping is observable verbatim.
  copied_run = utc_now_iso()
  cron_session.last_scheduled_run = copied_run
  await session_mgr.save_metadata(cron_session)

  with pytest.MonkeyPatch().context() as crash:
    if fail_after == "create":
      # The node exists; the bookkeeping copy never ran.
      crash.setattr(TaskTreeManager, "adopt_scheduled_bookkeeping", _boom)
    elif fail_after == "copy":
      # Node + bookkeeping exist; the write-back never ran.
      from src.core import scheduler as scheduler_module
      crash.setattr(scheduler_module, "write_cron_key", _boom)
    else:
      # The binding is on disk; the archive never ran (the next tick's sweep
      # covers exactly this).
      crash.setattr(Scheduler, "_archive_active_cron_sessions", _boom)

    # The tick reports the interrupted task's error and keeps going.
    await scheduler._tick()
  if fail_after in ("create", "copy"):
    # The binding never landed: the yaml is unchanged.
    assert "session_id" not in _read_task_yaml(home)
  else:
    # The binding landed; only the archive is missing.
    assert "session_id" in _read_task_yaml(home)
    stored = await session_mgr.get_session(cron_session.id)
    assert stored is not None and stored.status == SessionStatus.ACTIVE

  # The replayed tick lands the end state.
  await scheduler._tick()

  body = _read_task_yaml(home)
  node = await tree.load_meta(body["session_id"])
  assert node is not None and node.profile == "manager" and node.name == "nightly"
  # One task node only: the replay returned the original product (the legacy
  # cron session is a root too, but carries no profile).
  roots = [m for m in (await tree._get_index()).metas.values() if m.task_parent_id is None and m.profile == "manager"]
  assert [m.id for m in roots] == [node.id]
  # The migrated bookkeeping survived every replay path.
  assert node.last_scheduled_run == copied_run
  stored = await session_mgr.get_session(cron_session.id)
  assert stored is not None and stored.status == SessionStatus.ARCHIVED


# ---------------------------------------------------------------------------
# The sweep: an already-bound task keeps no active cron session
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_sweep_archives_active_cron_session_of_bound_task_despite_stuck_running_thread(tick_env) -> None:
  """An active cron session of an already-bound task \u2014 even one whose legacy
  thread is marked running \u2014 is archived by the next tick. No busy gate: a
  month-long scan window must not hold the archive off."""
  from conftest import write_thread_meta
  cfg, session_mgr, tree, scheduler, home = tick_env
  write_nightly_task(home)
  await scheduler._tick()  # binds; the daily task is not due
  node_id = _read_task_yaml(home)["session_id"]
  cron_session = await make_legacy_cron_session(session_mgr, "nightly")
  write_thread_meta(
      cfg, cron_session.id, {
          "id": "legacy-thread",
          "session_id": cron_session.id,
          "description": "legacy round",
          "status": "running",
          "pid": 999999,
      })

  await scheduler._tick()

  stored = await session_mgr.get_session(cron_session.id)
  assert stored is not None and stored.status == SessionStatus.ARCHIVED
  # The bound node is untouched by the sweep.
  node = await tree.load_meta(node_id)
  assert node is not None and node.last_scheduled_run is None


# ---------------------------------------------------------------------------
# Re-create: a task deleted and re-created under the same name reattaches
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_recreated_task_reattaches_to_its_original_node(tick_env) -> None:
  _cfg, session_mgr, tree, scheduler, home = tick_env
  write_nightly_task(home)
  await scheduler._tick()
  original_id = _read_task_yaml(home)["session_id"]
  # The user archived the node, then deleted the task and re-created it under
  # the same name without a binding.
  await tree.set_presentation(original_id, "hidden")
  (home / ".charliebot" / "config.d" / "cron.d" / "nightly.yaml").unlink()
  write_nightly_task(home)

  await scheduler._tick()

  body = _read_task_yaml(home)
  assert body["session_id"] == original_id  # the original node, not a second one
  node = await tree.load_meta(original_id)
  assert node is not None
  # The replayed node is unarchived: the task must not fire into a hidden node.
  fresh = await session_mgr.get_session(original_id)
  assert fresh is not None and fresh.status == SessionStatus.ACTIVE
  assert node.presentation == "shown"
  roots = [m for m in (await tree._get_index()).metas.values() if m.task_parent_id is None and m.profile == "manager"]
  assert [m.id for m in roots] == [original_id]


# ---------------------------------------------------------------------------
# Backend follows the task config
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_cron_editor_backend_change_switches_bound_node_in_place(
    tick_env, monkeypatch: pytest.MonkeyPatch) -> None:
  """A cron-editor backend change on a bound task creates no session and
  switches the node's backend in place."""
  cfg, session_mgr, tree, scheduler, home = tick_env
  _patch_cron_d(monkeypatch, home / ".charliebot" / "config.d" / "cron.d")
  write_nightly_task(home, backend=OPUS_BACKEND_ID)
  await scheduler._tick()
  node_id = _read_task_yaml(home)["session_id"]
  node = await tree.load_meta(node_id)
  assert node is not None and node.backend == OPUS_BACKEND_ID

  with make_cron_sessions_client(cfg, session_mgr, tree) as client:
    response = client.put("/api/cron/tasks/nightly", json={"backend": "codex-o3"})

  assert response.status_code == 200
  sessions = await session_mgr.list_sessions()
  assert len(sessions) == 1 and sessions[0].id == node_id  # no session was created
  node = await tree.load_meta(node_id)
  assert node is not None and node.backend == "codex-o3"
  assert _read_task_yaml(home)["backend"] == "codex-o3"


@pytest.mark.asyncio
async def test_cron_editor_backend_change_on_busy_node_409s_before_writing(
    tick_env, monkeypatch: pytest.MonkeyPatch) -> None:
  """The editor keeps its contract: 409 before any yaml write when the switch
  cannot happen now (the node's own work is in flight)."""
  from src.core.thinking_state import mark_busy
  cfg, session_mgr, tree, scheduler, home = tick_env
  _patch_cron_d(monkeypatch, home / ".charliebot" / "config.d" / "cron.d")
  write_nightly_task(home, backend=OPUS_BACKEND_ID)
  await scheduler._tick()
  node_id = _read_task_yaml(home)["session_id"]
  mark_busy(node_id)

  with make_cron_sessions_client(cfg, session_mgr, tree) as client:
    response = client.put("/api/cron/tasks/nightly", json={"backend": "codex-o3"})

  assert response.status_code == 409
  assert "running work" in response.json()["detail"]
  assert _read_task_yaml(home)["backend"] == OPUS_BACKEND_ID  # nothing written
  node = await tree.load_meta(node_id)
  assert node is not None and node.backend == OPUS_BACKEND_ID


@pytest.mark.asyncio
async def test_hand_edited_yaml_backend_is_followed_on_the_next_tick(tick_env) -> None:
  _cfg, session_mgr, tree, scheduler, home = tick_env
  write_nightly_task(home, backend=OPUS_BACKEND_ID)
  await scheduler._tick()
  node_id = _read_task_yaml(home)["session_id"]

  body = _read_task_yaml(home)
  body["backend"] = "codex-o3"
  _write_task_body(home, "nightly", body)

  await scheduler._tick()

  node = await tree.load_meta(node_id)
  assert node is not None and node.backend == "codex-o3"
  sessions = await session_mgr.list_sessions()
  assert [s.id for s in sessions] == [node_id]  # switched in place, no new session


# ---------------------------------------------------------------------------
# Lifecycle: archiving the node stops the task; deleting the task keeps the node
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_archiving_bound_node_writes_enabled_false(tick_env) -> None:
  """The user's archive action on a bound node stops its task: the single-key
  write flips only ``enabled`` in the task's yaml; the binding itself stays."""
  cfg, session_mgr, tree, scheduler, home = tick_env
  write_nightly_task(home, project="charlie")
  await scheduler._tick()
  node_id = _read_task_yaml(home)["session_id"]

  with make_cron_sessions_client(cfg, session_mgr, tree) as client:
    response = client.delete(f"/api/sessions/{node_id}")

  assert response.status_code == 200
  body = _read_task_yaml(home)
  assert body["enabled"] is False
  assert body["session_id"] == node_id  # the binding itself stays
  assert body["cron"] == "0 3 * * *" and body["project"] == "charlie"


@pytest.mark.asyncio
async def test_task_delete_leaves_its_node_active(tick_env) -> None:
  """Deleting the task unlinks the yaml only: the node stays active."""
  cfg, session_mgr, tree, scheduler, home = tick_env
  write_nightly_task(home)
  await scheduler._tick()
  node_id = _read_task_yaml(home)["session_id"]

  with make_cron_sessions_client(cfg, session_mgr, tree) as client:
    response = client.delete("/api/cron/tasks/nightly")

  assert response.status_code == 200
  assert response.json() == {"ok": True}
  assert not (home / ".charliebot" / "config.d" / "cron.d" / "nightly.yaml").exists()
  node = await tree.load_meta(node_id)
  assert node is not None
  fresh = await session_mgr.get_session(node_id)
  assert fresh is not None and fresh.status == SessionStatus.ACTIVE


# ---------------------------------------------------------------------------
# Wake duties: the bound node inherits the cron session's wake behavior on the
# tree dispatch path (the legacy trigger_master wake never reaches a task node)
# ---------------------------------------------------------------------------


def _backdate_cc_anchor(session_mgr: SessionManager, session_id: str, *, started_at: datetime) -> None:
  """Backdate the native anchor's started_at on disk.

  The anchor channels stamp only now, and a whole-object save is corrected
  back to the disk anchor by the save guard, so the file itself is the only
  honest way to age an anchor in a test.
  """
  path = session_mgr._metadata_path(session_id)
  body = json.loads(path.read_text(encoding="utf-8"))
  body["cc_session_started_at"] = started_at.isoformat()
  path.write_text(json.dumps(body), encoding="utf-8")
  session_mgr._metadata_cache.pop(session_id)  # the next read re-parses the file


def _write_old_thread(cfg: CharlieBotConfig, session_id: str, thread_id: str, completed_at: datetime) -> None:
  """One terminal thread predating the weekly recycle's cutoff."""
  thread_dir = cfg.sessions_dir / session_id / "threads" / thread_id
  thread_dir.mkdir(parents=True, exist_ok=True)
  (thread_dir / "metadata.json").write_text(
      ThreadMetadata(
          id=thread_id,
          session_id=session_id,
          description="legacy round",
          status=ThreadStatus.COMPLETED,
          completed_at=completed_at).model_dump_json(),
      encoding="utf-8")


@pytest.mark.asyncio
@pytest.mark.integration  # polls the fired run's terminal fact in real time; the weekly recycle walks the thread dirs
async def test_bound_node_wake_recycles_and_prefixes_the_firing_report(
    tick_env, monkeypatch: pytest.MonkeyPatch) -> None:
  """A bound node woken by a firing's child report takes over the cron
  session's wake duties on the tree dispatch path: the weekly recycle clears
  the anchor predating the last Saturday 01:00 PT and GCs the old threads,
  and the fresh native conversation's turn carries the fixed report prefix."""
  from src.core.master_trigger import scheduled_report_prefix
  cfg, session_mgr, tree, scheduler, home = tick_env
  write_nightly_task(home, backend=OPUS_BACKEND_ID)
  await scheduler._tick()  # binds; the daily task is not due
  node_id = _read_task_yaml(home)["session_id"]

  stub_credentials({"charliebot": {"access_key": "op-secret"}})
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
  backend = SpawningScriptedBackend([result_event("reviewed: merged")])
  install_backends(monkeypatch, [backend], BUILD_BACKEND_PATCH_TARGET)
  patch_instructions_content(monkeypatch)

  # An anchor started before the last Saturday 01:00 PT, and one old terminal
  # thread the weekly recycle must GC.
  await session_mgr.persist_cc_session_id(node_id, "cc-old")
  _backdate_cc_anchor(session_mgr, node_id, started_at=datetime.now(UTC) - timedelta(days=8))
  _write_old_thread(cfg, node_id, "legacy-round", datetime.now(UTC) - timedelta(days=8))

  await tree.dispatch.deliver_child_report(
      "worker-1", source_event={"id": "evt-1"}, outcome="completed", summary="sweep finished", recipient=node_id)
  decision = await tree.dispatch.dispatch_pending(node_id)
  assert decision["launch"] is True
  _run, outcome = await wait_for_terminal_run(tree, node_id, decision["run_id"])
  assert outcome == "success"

  # The prefix wrapped the firing report on the fresh native conversation.
  assert backend.prompt is not None
  assert backend.prompt.startswith(scheduled_report_prefix("nightly"))
  # The recycle ran: the stale anchor is cleared on disk and the old thread
  # is gone.
  disk = await session_mgr.read_metadata_fresh(node_id)
  assert disk.cc_session_id is None and disk.cc_session_started_at is None
  assert not (cfg.sessions_dir / node_id / "threads" / "legacy-round").exists()


@pytest.mark.asyncio
@pytest.mark.integration  # two dispatch turns, each polled in real time to its terminal fact
async def test_bound_node_wake_on_a_live_anchor_carries_no_prefix(tick_env, monkeypatch: pytest.MonkeyPatch) -> None:
  """The report prefix marks a FRESH native conversation: once the node holds
  a live anchor (recorded by the previous turn) the next firing report's wake
  continues that conversation \u2014 no prefix, no reset notice, no recycle."""
  from src.core.master_trigger import scheduled_report_prefix
  cfg, session_mgr, tree, scheduler, home = tick_env
  write_nightly_task(home, backend=OPUS_BACKEND_ID)
  await scheduler._tick()
  node_id = _read_task_yaml(home)["session_id"]

  stub_credentials({"charliebot": {"access_key": "op-secret"}})
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
  builds = install_backends(
      monkeypatch,
      [SpawningScriptedBackend([result_event("first")]),
       SpawningScriptedBackend([result_event("second")])], BUILD_BACKEND_PATCH_TARGET)
  patch_instructions_content(monkeypatch)

  # Turn 1: a firing report wakes the anchor-less node — a fresh native
  # conversation carrying the prefix; the launch records the native anchor.
  await tree.dispatch.deliver_child_report(
      "worker-1", source_event={"id": "evt-1"}, outcome="completed", summary="first sweep", recipient=node_id)
  decision = await tree.dispatch.dispatch_pending(node_id)
  assert decision["launch"] is True
  _run1, outcome1 = await wait_for_terminal_run(tree, node_id, decision["run_id"])
  assert outcome1 == "success"
  assert builds[0]["backend"].prompt.startswith(scheduled_report_prefix("nightly"))

  # A now-valid anchor on the same native identity: the conversation continues.
  await session_mgr.persist_cc_session_id(node_id, "cc-live")
  disk = await session_mgr.read_metadata_fresh(node_id)
  assert disk.native_prompt_hash is not None and disk.native_backend == OPUS_BACKEND_ID

  await tree.dispatch.deliver_child_report(
      "worker-2", source_event={"id": "evt-2"}, outcome="completed", summary="second sweep", recipient=node_id)
  decision2 = await tree.dispatch.dispatch_pending(node_id)
  assert decision2["launch"] is True
  _run2, outcome2 = await wait_for_terminal_run(tree, node_id, decision2["run_id"])
  assert outcome2 == "success"
  # The turn 2 prompt carried neither the scheduled prefix nor a reset notice:
  # the same native conversation continued.
  prompt2 = builds[1]["backend"].prompt
  assert prompt2 is not None
  assert scheduled_report_prefix("nightly") not in prompt2
  assert "[Context reset" not in prompt2


# ---------------------------------------------------------------------------
# The scheduled-task read paths keep working off the migrated state
# ---------------------------------------------------------------------------


def test_task_config_model_still_carries_the_fields_the_lists_read() -> None:
  """The row schedule fields (delivery 2) read the task config; the binding is
  one of its fields and the load-time mode rule still references it."""
  task = ScheduledTaskConfig(name="nightly", cron="0 3 * * *", prompt="x", session_id="s")
  assert task.session_id == "s"
  with pytest.raises(ValueError, match=r"mode.*requires.*session_id"):
    ScheduledTaskConfig(name="nightly", cron="0 3 * * *", prompt="x", mode="master")
  # A past timestamp the migrated bookkeeping may carry parses as expected.
  assert datetime.fromisoformat("2026-06-07T09:00:00+00:00").tzinfo is not None
