"""Bound-scheduled-task tests: one stable node, durable firing identity, Runs.

Exercises the actual scheduler entry points (_maybe_run / run_task_now) against
a synthetic instance with deterministic scripted backend processes: bound
master and worker modes, steps sharing one leaf, per-step backends, stop
semantics, config API round-trip, closed/paused nodes, new-vs-replayed
firings, no role/group manager creation, and run-token spoof attempts.
"""

from __future__ import annotations

import asyncio
import json as _json
import uuid
from pathlib import Path

import pytest
import yaml
from conftest import CODEX_BACKEND_OPTION, OPUS_BACKEND_ID, OPUS_BACKEND_OPTION

from src.core import event_types as ET
from src.core.config import CharlieBotConfig, ScheduledTaskConfig, StepConfig
from src.core.control_events import stable_run_id
from src.core.models import RunRecord, SessionMetadata, SessionStatus, TaskSpec
from src.core.scheduler import Scheduler
from src.core.sessions import SessionManager
from src.core.task_sessions import TaskTreeManager
from tests.test_task_execution import (
  SpawningScriptedBackend,
  _adapter_with_silent_broadcast,
  install_backends,
  result_event,
)


def build_env(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
  """One synthetic home, the fake + codex backends, and one task tree."""
  import src.core.config as core_config
  home = tmp_path / "charliebot-home"
  cfg = CharlieBotConfig(
      charliebot_home=home,
      backends={"options": [
          OPUS_BACKEND_OPTION,
          CODEX_BACKEND_OPTION,
          {"id": "fake", "label": "Fake", "type": "codex", "model": "fake-model"},
      ]},
      paths={"worktree_dir": str(home / "worktrees")})
  core_config._credentials_cache.seed(core_config.Credentials(
      path=home / "credentials.yaml", sections={"charliebot": {"access_key": "cron-seq-key"}}))
  monkeypatch.setenv("CHARLIEBOT_HOME", str(home))
  session_mgr = SessionManager(cfg)
  return cfg, session_mgr, TaskTreeManager(cfg, session_mgr)


@pytest.fixture()
def bound_env(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
  """cfg + tree with the scheduler's deps singleton and the adapter installed.

  The bound manager node is created per test (each test is async).
  """
  cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
  from src.api import deps
  monkeypatch.setattr(deps, "_task_manager", tree)
  monkeypatch.setattr(deps, "_session_manager", session_mgr)
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
  return cfg, session_mgr, tree


async def make_manager(tree: TaskTreeManager, name: str = "PM"):
  return await tree.create_task(
      request_id=f"pm-{name}", task_parent_id=None, profile="manager",
      task=TaskSpec(goal="pm"), name=name, backend=None, caller="operator")


def _bound_task(name: str, session_id: str, **overrides) -> ScheduledTaskConfig:
  body = {
      "name": name,
      "cron": "0 3 * * *",
      "session_id": session_id,
      "backend": "fake",
  }
  body.update(overrides)
  return ScheduledTaskConfig(**body)


# ---------------------------------------------------------------------------
# Master mode: the typed scheduled input lands on the bound manager
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_bound_master_admits_one_typed_input_and_dispatches(
        bound_env, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_mgr, tree = bound_env
  manager = await make_manager(tree)
  builds = install_backends(monkeypatch, [SpawningScriptedBackend([result_event("awake")])],
                            "src.agents.backends.registry.build_backend")
  from conftest import patch_instructions_content
  patch_instructions_content(monkeypatch)
  task_cfg = _bound_task("wake-pm", manager.id, mode="master", prompt="Standup time.")
  scheduler = Scheduler(cfg, session_mgr)

  result = await scheduler._execute_task(task_cfg, record_handle=True, firing="2026-01-01T03:00:00+00:00")
  assert result["session_id"] == manager.id
  decision = await tree.dispatch.dispatch_pending(manager.id)
  assert decision["launch"] is True
  run_id = decision["run_id"]
  deadline = asyncio.get_event_loop().time() + 10
  while asyncio.get_event_loop().time() < deadline:
    run = await tree.runs.get_run(manager.id, run_id)
    if run is not None and tree.runs.terminal_outcome(
        tree.runs.load_events_sync(manager.id), run_id) is not None:
      break
    await asyncio.sleep(0.05)
  events = tree.events.load_events(manager.id)
  triggers = [e for e in events if e.get("type") == ET.SCHEDULED_TRIGGER]
  assert len(triggers) == 1
  assert triggers[0].get("id") == "cron:wake-pm:2026-01-01T03:00:00+00:00"
  assert "Standup time." in str(triggers[0].get("content"))
  # No legacy Group line rides a bound fire (no role/group discovery here).
  assert "Group:" not in str(triggers[0].get("content"))
  # The manager turn consumed it as its serialized batch; no process duplication.
  assert len(builds) == 1
  assert len(tree.runs.list_run_records_sync(manager.id)) == 1

  # A replayed fire at the same firing identity re-admits nothing.
  await scheduler._execute_task(task_cfg, record_handle=True, firing="2026-01-01T03:00:00+00:00")
  events = tree.events.load_events(manager.id)
  assert len([e for e in events if e.get("type") == ET.SCHEDULED_TRIGGER]) == 1
  # An intentional new firing is a distinct input.
  await scheduler._execute_task(task_cfg, record_handle=True, firing="2026-01-02T03:00:00+00:00")
  events = tree.events.load_events(manager.id)
  assert len([e for e in events if e.get("type") == ET.SCHEDULED_TRIGGER]) == 2


@pytest.mark.asyncio
async def test_bound_master_worker_spoof_cannot_forged_scheduled_input(
        bound_env, monkeypatch: pytest.MonkeyPatch) -> None:
  """The scheduled input's provenance is server-owned: a run-token caller
  cannot mint SCHEDULED_TRIGGER events, and its real messages stay agent ones."""
  cfg, session_mgr, tree = bound_env
  manager = await make_manager(tree)
  run_id = stable_run_id(manager.id, "spoof:work")
  await tree.runs.register_run(RunRecord(id=run_id, session_id=manager.id, kind="work"))
  await tree.runs.record_launch(manager.id, run_id, pid=424000, pid_start="ps-1")
  import src.core.config as core_config
  from src.core.run_token import RunTokenClaims, sign_run_token
  key = str(core_config.get_credentials().get("charliebot", "access_key") or "")
  claims = RunTokenClaims(session_id=manager.id, run_id=run_id, agent="PM")
  token = sign_run_token(claims, key)
  request = type("R", (), {"headers": {"authorization": f"Bearer {token}"}})()
  from src.api.deps import require_caller
  identity = await require_caller(request, tree.runs)
  assert identity.is_operator is False
  # The verified agent caller relays only agent messages; a scheduled input's
  # provenance is the server's (actor=system on the scheduler's own fire), and
  # the dispatcher refuses the SCHEDULED_TRIGGER type from a caller entirely.
  with pytest.raises(Exception, match="scheduler mints scheduled triggers"):
    await tree.dispatch.admit_input(
        manager.id, event_type=ET.SCHEDULED_TRIGGER,
        content="forged standup", actor="agent", input_id="spoofed-input")
  # And a USER event from a non-operator actor is refused (the run token can
  # never fabricate the take-off time either).
  with pytest.raises(Exception, match="real user message"):
    await tree.dispatch.admit_input(
        manager.id, event_type=ET.USER,
        content="Take off.", actor="agent", input_id="spoofed-user")


# ---------------------------------------------------------------------------
# Worker modes: one leaf per firing, steps share it, one boundary report
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_bound_steps_one_leaf_ordered_runs_one_report(
        bound_env, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_mgr, tree = bound_env
  manager = await make_manager(tree)
  builds = install_backends(monkeypatch, [
      SpawningScriptedBackend([result_event("selector says pick three")]),
      SpawningScriptedBackend([result_event("reviewer wrote the report")]),
  ], "src.agents.worker.build_backend")
  task_cfg = _bound_task(
      "chained", manager.id,
      steps=[
          StepConfig(name="selector", prompt="Select candidates."),
          StepConfig(name="reviewer", prompt="Review the diff.", backend="codex-o3"),
      ])
  scheduler = Scheduler(cfg, session_mgr)
  firing = "2026-01-01T03:00:00+00:00"
  result = await scheduler._execute_task(task_cfg, record_handle=True, firing=firing)
  leaf_id = result["leaf_session_id"]
  deadline = asyncio.get_event_loop().time() + 20
  while asyncio.get_event_loop().time() < deadline:
    records = tree.runs.list_run_records_sync(leaf_id)
    reports = [e for e in tree.events.load_events(manager.id) if e.get("type") == ET.CHILD_REPORT]
    if len(records) == 2 and reports and all(
        tree.runs.terminal_outcome(tree.runs.load_events_sync(leaf_id), r.id) is not None
        for r in records):
      break
    await asyncio.sleep(0.1)
  else:
    pytest.fail(f"the firing never finished: {[(r.kind, r.sequence_ref and r.sequence_ref.position) for r in tree.runs.list_run_records_sync(leaf_id)]}")

  # ONE leaf per firing; both steps are ordered scheduled_step Runs on it.
  children = []
  for p in sorted((cfg.sessions_dir).glob("*/metadata.json")):
    meta = SessionMetadata.model_validate_json(p.read_text())
    if meta.task_parent_id == manager.id:
      children.append(meta.id)
  assert children == [leaf_id]
  records = tree.runs.list_run_records_sync(leaf_id)
  assert [r.kind for r in records] == ["scheduled_step", "scheduled_step"]
  assert [r.sequence_ref.position for r in records] == [0, 1]
  assert [r.sequence_ref.kind for r in records] == ["cron_steps", "cron_steps"]
  assert all(r.sequence_ref.owner_ref == f"cron:chained:{firing}" for r in records)
  # Per-step backends honored: step 0 rode the task backend, step 1 its own.
  assert records[0].backend == "fake"
  assert records[1].backend == "codex-o3"
  # The second step's prompt carried the previous result under the legacy heading.
  assert "Result of the previous step (selector)" in builds[1]["backend"].prompt
  assert "selector says pick three" in builds[1]["backend"].prompt
  # ONE report at the sequence boundary, delivered to the bound manager; the
  # fully-successful chain closed the leaf through the common completion owner.
  reports = [e for e in tree.events.load_events(manager.id) if e.get("type") == ET.CHILD_REPORT]
  assert len(reports) == 1
  assert "completed all 2 step(s)" in str(reports[0].get("summary"))
  assert "selector" in str(reports[0].get("summary")) and "reviewer" in str(reports[0].get("summary"))
  assert tree.task_state(leaf_id) == "completed"

  # A replayed fire at the same firing identity creates nothing new.
  await scheduler._execute_task(task_cfg, record_handle=True, firing=firing)
  assert len(tree.runs.list_run_records_sync(leaf_id)) == 2
  children2 = []
  for p in sorted((cfg.sessions_dir).glob("*/metadata.json")):
    meta = SessionMetadata.model_validate_json(p.read_text())
    if meta.task_parent_id == manager.id:
      children2.append(meta.id)
  assert children2 == [leaf_id]


@pytest.mark.asyncio
async def test_bound_steps_failure_stops_chain_and_reports_failed(
        bound_env, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_mgr, tree = bound_env
  manager = await make_manager(tree)
  install_backends(monkeypatch, [
      SpawningScriptedBackend([result_event("broke")], exit_code=1),
  ], "src.agents.worker.build_backend")
  task_cfg = _bound_task(
      "chained", manager.id,
      steps=[
          StepConfig(name="selector", prompt="Select."),
          StepConfig(name="reviewer", prompt="Review."),
      ])
  # The recovery reads the binding from the durable cron.d file (scoped to
  # this instance's home), so persist it before the simulated restart.
  cron_d = cfg.charliebot_home / "config.d" / "cron.d"
  cron_d.mkdir(parents=True, exist_ok=True)
  sel_md = cron_d / "selector.md"
  rev_md = cron_d / "reviewer.md"
  sel_md.write_text("Select the target.\n", encoding="utf-8")
  rev_md.write_text("Review the result.\n", encoding="utf-8")
  (cron_d / "chained.yaml").write_text(yaml.safe_dump({
      "cron": "0 3 * * *",
      "session_id": manager.id,
      "backend": "fake",
      "steps": [
          {"name": "selector", "prompt_file": str(sel_md)},
          {"name": "reviewer", "prompt_file": str(rev_md)},
      ],
  }), encoding="utf-8")
  scheduler = Scheduler(cfg, session_mgr)
  firing = "2026-01-01T03:00:00+00:00"
  result = await scheduler._execute_task(task_cfg, record_handle=True, firing=firing)
  leaf_id = result["leaf_session_id"]
  deadline = asyncio.get_event_loop().time() + 20
  while asyncio.get_event_loop().time() < deadline:
    reports = [e for e in tree.events.load_events(manager.id) if e.get("type") == ET.CHILD_REPORT]
    if reports:
      break
    await asyncio.sleep(0.1)
  else:
    pytest.fail("the failure report never arrived")
  # The chain stopped at the failed step: no later step run exists.
  records = tree.runs.list_run_records_sync(leaf_id)
  assert [r.sequence_ref.position for r in records] == [0]
  # The failed step's report names the stop and the leaf stays open (attention).
  assert "stopped at step 'selector'" in str(reports[0].get("summary"))
  assert tree.task_state(leaf_id) != "completed"


@pytest.mark.asyncio
async def test_bound_prompt_task_one_leaf_per_firing_and_distinct_firings(
        bound_env, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_mgr, tree = bound_env
  manager = await make_manager(tree)
  install_backends(monkeypatch, [
      SpawningScriptedBackend([result_event("round one")]),
      SpawningScriptedBackend([result_event("round two")]),
  ], "src.agents.worker.build_backend")
  task_cfg = _bound_task("daily-nudge", manager.id, prompt="Do the nudge.")
  scheduler = Scheduler(cfg, session_mgr)
  r1 = await scheduler._execute_task(task_cfg, record_handle=True, firing="2026-01-01T03:00:00+00:00")
  leaf1 = r1["leaf_session_id"]
  deadline = asyncio.get_event_loop().time() + 15
  while asyncio.get_event_loop().time() < deadline:
    reports = [e for e in tree.events.load_events(manager.id) if e.get("type") == ET.CHILD_REPORT]
    if reports:
      break
    await asyncio.sleep(0.1)
  else:
    pytest.fail("the first firing's report never arrived")
  # A distinct firing creates a DISTINCT leaf; the first one closed on success.
  r2 = await scheduler._execute_task(task_cfg, record_handle=True, firing="2026-01-02T03:00:00+00:00")
  leaf2 = r2["leaf_session_id"]
  assert leaf2 != leaf1
  # Two leaves total under the manager, one per firing; the first auto-closed.
  children = []
  for p in sorted((cfg.sessions_dir).glob("*/metadata.json")):
    meta = SessionMetadata.model_validate_json(p.read_text())
    if meta.task_parent_id == manager.id:
      children.append(meta.id)
  assert sorted(children) == sorted([leaf1, leaf2])
  assert tree.task_state(leaf1) == "completed"


# ---------------------------------------------------------------------------
# Binding validation: never a replacement session, closed/paused skip
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_missing_binding_fails_visibly_without_creating_a_session(
        bound_env, monkeypatch: pytest.MonkeyPatch) -> None:
  from src.core.cron_sequence import ScheduledBindingError
  cfg, session_mgr, tree = bound_env
  await make_manager(tree)
  task_cfg = _bound_task("ghost", str(uuid.uuid4()), mode="master", prompt="Nobody home.")
  scheduler = Scheduler(cfg, session_mgr)
  with pytest.raises(ScheduledBindingError, match="does not exist"):
    await scheduler._execute_task(task_cfg, record_handle=True, firing="2026-01-01T03:00:00+00:00")
  # No session was created for the missing binding.
  metas = list(cfg.sessions_dir.glob("*/metadata.json"))
  assert len(metas) == 1  # only the manager itself


@pytest.mark.asyncio
async def test_legacy_session_binding_refuses_the_v2_path(
        bound_env, monkeypatch: pytest.MonkeyPatch) -> None:
  from src.core.cron_sequence import ScheduledBindingError
  cfg, session_mgr, tree = bound_env
  await make_manager(tree)
  from src.core.models import CreateSessionRequest
  legacy = await session_mgr.create_session(
      CreateSessionRequest(name="Old PM"), backend=OPUS_BACKEND_ID)
  task_cfg = _bound_task("legacy-bound", legacy.id, mode="master", prompt="wake")
  scheduler = Scheduler(cfg, session_mgr)
  with pytest.raises(ScheduledBindingError, match="not a task-tree node"):
    await scheduler._execute_task(task_cfg, record_handle=True, firing="2026-01-01T03:00:00+00:00")


@pytest.mark.asyncio
async def test_closed_bound_node_generates_no_new_execution(
        bound_env, monkeypatch: pytest.MonkeyPatch) -> None:
  from src.core.cron_sequence import ScheduledBindingError
  cfg, session_mgr, tree = bound_env
  manager = await make_manager(tree)
  install_backends(monkeypatch, [SpawningScriptedBackend([result_event("x")])],
                   "src.agents.backends.registry.build_backend")
  from src.core.run_token import CallerIdentity
  from src.core.task_completion import CompletionEvidence
  close_run = stable_run_id(manager.id, "close:evidence")
  await tree.runs.register_run(RunRecord(id=close_run, session_id=manager.id, kind="manager_turn"))
  await tree.runs.record_launch(manager.id, close_run, pid=424001, pid_start="ps-1")
  await tree.dispatch.finish_run(manager.id, close_run, outcome="success")
  await tree.completion.complete_task(
      manager.id, request_id="close-1", caller=CallerIdentity(kind="operator"),
      evidence=CompletionEvidence(
          summary="done", run_ids=[close_run],
          result_refs=[f"run:{close_run}"]))
  task_cfg = _bound_task("wake-pm", manager.id, mode="master", prompt="Standup.")
  scheduler = Scheduler(cfg, session_mgr)
  with pytest.raises(ScheduledBindingError, match="no new cron execution"):
    await scheduler._execute_task(task_cfg, record_handle=True, firing="2026-01-01T03:00:00+00:00")
  # The configuration remains readable.
  assert task_cfg.session_id == manager.id


@pytest.mark.asyncio
async def test_paused_bound_node_generates_no_new_execution(
        bound_env, monkeypatch: pytest.MonkeyPatch) -> None:
  from src.core.cron_sequence import ScheduledBindingError
  cfg, session_mgr, tree = bound_env
  manager = await make_manager(tree)
  builds = install_backends(monkeypatch, [SpawningScriptedBackend([result_event("x")])],
                            "src.agents.backends.registry.build_backend")
  from src.core.models import PatchSessionTaskRequest
  from src.core.run_token import CallerIdentity
  await tree.patch_task(
      manager.id, PatchSessionTaskRequest(automation_paused=True),
      caller=CallerIdentity(kind="operator"))
  task_cfg = _bound_task("wake-pm", manager.id, mode="master", prompt="Standup.")
  scheduler = Scheduler(cfg, session_mgr)
  with pytest.raises(ScheduledBindingError, match="paused"):
    await scheduler._execute_task(task_cfg, record_handle=True, firing="2026-01-01T03:00:00+00:00")
  assert builds == []


# ---------------------------------------------------------------------------
# Config schema + API round-trip
# ---------------------------------------------------------------------------


def test_scheduled_config_carries_and_validates_binding(tmp_path: Path) -> None:
  CharlieBotConfig(charliebot_home=tmp_path / "h", backends={"options": [OPUS_BACKEND_OPTION]})
  task = ScheduledTaskConfig(name="t", cron="0 3 * * *", session_id="abc", prompt="wake", backend=OPUS_BACKEND_ID)
  assert task.session_id == "abc"
  # A bound task may not also declare role/group discovery.
  from src.core.config import scheduled_binding_error
  assert scheduled_binding_error(task) is None
  with pytest.raises(ValueError, match="must not also declare 'project'"):
    ScheduledTaskConfig(name="t", cron="0 3 * * *", session_id="abc", project="web",
                        prompt="wake", backend=OPUS_BACKEND_ID)
  # And the mode-master + explicit binding combination is legal without a project.
  bound_master = ScheduledTaskConfig(
      name="t", cron="0 3 * * *", session_id="abc", mode="master", prompt="wake",
      backend=OPUS_BACKEND_ID)
  assert bound_master.mode == "master" and bound_master.session_id == "abc"


@pytest.mark.asyncio
async def test_config_loader_and_api_round_trip_binding(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  from src.api import cron as cron_api
  from src.core.config import _load_cron_file
  cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
  manager = await tree.create_task(
      request_id="pm", task_parent_id=None, profile="manager",
      task=TaskSpec(goal="pm"), name="PM", backend=None, caller="operator")
  cron_dir = cfg.charliebot_home / "config.d" / "cron.d"
  cron_dir.mkdir(parents=True, exist_ok=True)
  wake_md = cron_dir / "wake.md"
  wake_md.write_text("wake the pm\n", encoding="utf-8")
  (cron_dir / "bound.yaml").write_text(yaml.safe_dump({
      "cron": "0 3 * * *",
      "prompt_file": str(wake_md),
      "session_id": str(manager.id),
      "backend": "fake",
  }), encoding="utf-8")
  task, _mtimes = _load_cron_file(cron_dir / "bound.yaml", cfg.charlie_bot_repo, "bound")
  assert task.session_id == str(manager.id)
  assert task.prompt == "wake the pm\n"

  # The API update round-trips the binding (set, overwrite, clear).
  raw = task.model_dump()
  updated = cron_api._apply_task_update(raw, cron_api.TaskUpdate(session_id="other-id"))
  assert updated["session_id"] == "other-id"
  cleared = cron_api._apply_task_update(updated, cron_api.TaskUpdate(session_id=None))
  assert "session_id" not in cleared
  # The update result reloads through the same schema (validation holds).
  reloaded = ScheduledTaskConfig(**{**cleared, "backend": "fake"})
  assert reloaded.session_id is None


@pytest.mark.asyncio
async def test_recovery_redrives_a_mid_chain_firing_from_durable_facts(
        bound_env, monkeypatch: pytest.MonkeyPatch) -> None:
  """A restart between step 0's terminal fact and the controller's advance:
  recovery launches the next position through the same launch checks and the
  chain ends with exactly ONE boundary report; a repeated pass adds nothing."""
  from src.core.control_events import stable_run_id
  from src.core.task_recovery import reconcile_task_tree
  cfg, session_mgr, tree = bound_env
  manager = await make_manager(tree)
  install_backends(monkeypatch, [
      SpawningScriptedBackend([result_event("selector done")]),
      SpawningScriptedBackend([result_event("reviewer done")]),
  ], "src.agents.worker.build_backend")
  task_cfg = _bound_task(
      "chained", manager.id,
      steps=[
          StepConfig(name="selector", prompt="Select."),
          StepConfig(name="reviewer", prompt="Review."),
      ])
  # The recovery reads the binding from the durable cron.d file (scoped to
  # this instance's home), so persist it before the simulated restart.
  cron_d = cfg.charliebot_home / "config.d" / "cron.d"
  cron_d.mkdir(parents=True, exist_ok=True)
  sel_md = cron_d / "selector.md"
  rev_md = cron_d / "reviewer.md"
  sel_md.write_text("Select the target.\n", encoding="utf-8")
  rev_md.write_text("Review the result.\n", encoding="utf-8")
  (cron_d / "chained.yaml").write_text(yaml.safe_dump({
      "cron": "0 3 * * *",
      "session_id": manager.id,
      "backend": "fake",
      "steps": [
          {"name": "selector", "prompt_file": str(sel_md)},
          {"name": "reviewer", "prompt_file": str(rev_md)},
      ],
  }), encoding="utf-8")
  scheduler = Scheduler(cfg, session_mgr)
  firing = "2026-01-01T03:00:00+00:00"
  result = await scheduler._execute_task(task_cfg, record_handle=True, firing=firing)
  leaf_id = result["leaf_session_id"]

  # Hand-craft the post-restart mid-chain state: step 0 ran to its durable
  # success; the controller (dead with the old process) never advanced, no
  # step 1 exists, and no boundary report was delivered. The child's create
  # task and step runs are removed of their manager-side reports.
  from src.core.chat_events import chat_events_path

  def drop_facts(session_id: str, *drop_types: str) -> None:
    path = chat_events_path(cfg.sessions_dir / session_id)
    kept = [e for e in tree.events.load_events(session_id)
            if e.get("type") not in drop_types]
    path.write_text("".join(_json.dumps(e) + "\n" for e in kept), encoding="utf-8")
    session_mgr._chat_events.clear_cache(session_id)

  # Hand-craft the post-restart mid-chain state: the first controller task is
  # cancelled at the boundary (the fire-time handle), so step 0's durable
  # success is all that exists — no advance, no step 1, no report, no close.
  handle = asyncio.all_tasks()  # no-op guard; the real cancellation is below
  del handle
  leaf_meta = await tree.load_meta(leaf_id)
  assert leaf_meta is not None

  async def _cancel_controller_when_step0_done():
    step0_id = stable_run_id(leaf_id, f"cron:chained:{firing}:step:0")
    deadline = asyncio.get_event_loop().time() + 20
    while asyncio.get_event_loop().time() < deadline:
      if tree.runs.terminal_outcome(
          tree.runs.load_events_sync(leaf_id), step0_id) is not None:
        for task in asyncio.all_tasks():
          if task.get_name() == f"bound_steps_chained_{firing}":
            task.cancel()
            return
        return
      await asyncio.sleep(0.05)

  watcher = asyncio.get_event_loop().create_task(_cancel_controller_when_step0_done())
  deadline = asyncio.get_event_loop().time() + 20
  while asyncio.get_event_loop().time() < deadline:
    records = tree.runs.list_run_records_sync(leaf_id)
    if len(records) == 1:
      break
    await asyncio.sleep(0.1)
  await asyncio.wait_for(watcher, 20)
  # Whatever the controller delivered before the cancel is dropped: the state
  # is exactly "step 0 terminal, nothing else".
  drop_facts(manager.id, ET.CHILD_REPORT)
  drop_facts(leaf_id, ET.TASK_CLOSED)
  fresh_leaf = await tree.load_meta(leaf_id)
  assert fresh_leaf is not None
  fresh_leaf.status = SessionStatus.ACTIVE
  await session_mgr.save_metadata(fresh_leaf)

  # Pass 1: the recovery launches the next position (the fact-driven frontier);
  # the boundary report only lands once that step reaches its terminal fact.
  await reconcile_task_tree(cfg, tree, session_mgr)
  deadline = asyncio.get_event_loop().time() + 20
  while asyncio.get_event_loop().time() < deadline:
    records = tree.runs.list_run_records_sync(leaf_id)
    if len(records) == 2 and all(
        tree.runs.terminal_outcome(tree.runs.load_events_sync(leaf_id), r.id) is not None
        for r in records):
      break
    await asyncio.sleep(0.1)
  else:
    pytest.fail("the recovered step never reached its terminal fact")
  records = tree.runs.list_run_records_sync(leaf_id)
  assert sorted(r.sequence_ref.position for r in records if r.sequence_ref) == [0, 1]
  assert [e for e in tree.events.load_events(manager.id)
          if e.get("type") == ET.CHILD_REPORT] == []
  # Pass 2: the finished chain's ONE boundary report is re-delivered and the
  # leaf closes (the stable report/close ids dedup any later replay).
  await reconcile_task_tree(cfg, tree, session_mgr)
  reports = [e for e in tree.events.load_events(manager.id) if e.get("type") == ET.CHILD_REPORT]
  assert len(reports) == 1, f"expected exactly one boundary report: {reports}"
  # A repeated pass adds nothing (no second step, no second report).
  await reconcile_task_tree(cfg, tree, session_mgr)
  assert len(tree.runs.list_run_records_sync(leaf_id)) == 2
  assert len([e for e in tree.events.load_events(manager.id)
              if e.get("type") == ET.CHILD_REPORT]) == 1
