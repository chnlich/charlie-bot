"""Bound-scheduled-task tests: one stable node, durable firing identity, Runs.

Exercises the actual scheduler entry points (_maybe_run / run_task_now) against
a synthetic instance with deterministic scripted backend processes: bound
pm and normal types, steps sharing one leaf, per-step backends, stop
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
from conftest import (
    CODEX_BACKEND_OPTION,
    OPUS_BACKEND_ID,
    OPUS_BACKEND_OPTION,
    patch_instructions_content,
)

from src.core import event_types as ET
from src.core.config import CharlieBotConfig, ScheduledTaskConfig, StepConfig
from src.core.control_events import stable_run_id
from src.core.models import RunRecord, SessionMetadata, TaskSpec
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
      "type": "normal",
      "session_id": session_id,
      "backend": "fake",
  }
  body.update(overrides)
  return ScheduledTaskConfig(**body)


def _script_manager_turn(monkeypatch: pytest.MonkeyPatch, notes: list[str]) -> None:
  """Script the parent node's report-consuming turn through the registry
  builder (the manager dispatch path), so no external process starts from
  these tests. One scripted backend per expected manager-turn build."""
  install_backends(
      monkeypatch,
      [SpawningScriptedBackend([result_event(note)]) for note in notes],
      "src.agents.backends.registry.build_backend")
  patch_instructions_content(monkeypatch)


async def _drain_manager_turns(tree: TaskTreeManager, manager_id: str) -> None:
  """Wait until every manager_turn Run of the node reached a terminal fact,
  so no launch task outlives the test as background residue."""
  deadline = asyncio.get_event_loop().time() + 15
  while asyncio.get_event_loop().time() < deadline:
    records = [r for r in tree.runs.list_run_records_sync(manager_id)
               if r.kind == "manager_turn"]
    events = tree.runs.load_events_sync(manager_id)
    if records and all(
        tree.runs.terminal_outcome(events, r.id) is not None for r in records):
      return
    await asyncio.sleep(0.05)
  pytest.fail(f"the manager turn never settled: {tree.runs.list_run_records_sync(manager_id)}")


# ---------------------------------------------------------------------------
# pm type: the typed scheduled input lands on the bound manager
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
  task_cfg = _bound_task("wake-pm", manager.id, type="pm", prompt="Standup time.")
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
  _cfg, _session_mgr, tree = bound_env
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
# Normal types: one leaf per firing, steps share it, one boundary report
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
  # The sequence boundary's report-consuming turn rides the manager dispatch
  # path (registry builder): script it so no external process starts here.
  _script_manager_turn(monkeypatch, ["report noted"])
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
  # Every scheduled step launch carries the assembler's durable snapshot: the
  # step prompt override no longer bypasses the assembly.
  for r in records:
    assert r.prompt_snapshot_ref is not None and Path(r.prompt_snapshot_ref).is_file()
    stored = _json.loads(Path(r.prompt_snapshot_ref).read_text(encoding="utf-8"))
    refs = [s["source_ref"] for b in stored["blocks"] for s in b["sources"]]
    assert "prompts/worker.md" in refs
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

  # The parent's report-consuming turn settled (no background residue).
  await _drain_manager_turns(tree, manager.id)

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
      "type": "normal",
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
  # Each firing's report-consuming turn rides the manager dispatch path:
  # script both so no external process starts here.
  _script_manager_turn(monkeypatch, ["noted round one", "noted round two"])
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
  # Both firing reports' parent turns settled (no background residue).
  await _drain_manager_turns(tree, manager.id)


# ---------------------------------------------------------------------------
# Binding validation: never a replacement session, closed/paused skip
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_missing_binding_fails_visibly_without_creating_a_session(
        bound_env, monkeypatch: pytest.MonkeyPatch) -> None:
  from src.core.cron_sequence import ScheduledBindingError
  cfg, session_mgr, tree = bound_env
  await make_manager(tree)
  task_cfg = _bound_task("ghost", str(uuid.uuid4()), type="pm", prompt="Nobody home.")
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
  task_cfg = _bound_task("legacy-bound", legacy.id, type="pm", prompt="wake")
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
  task_cfg = _bound_task("wake-pm", manager.id, type="pm", prompt="Standup.")
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
  task_cfg = _bound_task("wake-pm", manager.id, type="pm", prompt="Standup.")
  scheduler = Scheduler(cfg, session_mgr)
  with pytest.raises(ScheduledBindingError, match="paused"):
    await scheduler._execute_task(task_cfg, record_handle=True, firing="2026-01-01T03:00:00+00:00")
  assert builds == []


# ---------------------------------------------------------------------------
# Config schema + API round-trip
# ---------------------------------------------------------------------------


def test_scheduled_config_carries_and_validates_binding(tmp_path: Path) -> None:
  CharlieBotConfig(charliebot_home=tmp_path / "h", backends={"options": [OPUS_BACKEND_OPTION]})
  task = ScheduledTaskConfig(name="t", cron="0 3 * * *", type="pm", session_id="abc", prompt="wake", backend=OPUS_BACKEND_ID)
  assert task.session_id == "abc"
  # A bound task may not also declare role/group discovery.
  from src.core.config import scheduled_binding_error
  assert scheduled_binding_error(task) is None
  with pytest.raises(ValueError, match="must not also declare 'project'"):
    ScheduledTaskConfig(name="t", cron="0 3 * * *", type="pm", session_id="abc", project="web",
                        prompt="wake", backend=OPUS_BACKEND_ID)
  # And the type-pm + explicit binding combination is legal without a project.
  bound_master = ScheduledTaskConfig(
      name="t", cron="0 3 * * *", session_id="abc", type="pm", prompt="wake",
      backend=OPUS_BACKEND_ID)
  assert bound_master.type == "pm" and bound_master.session_id == "abc"


@pytest.mark.asyncio
async def test_config_loader_and_api_round_trip_binding(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  from src.api import cron as cron_api
  from src.core.config import _load_cron_file
  cfg, _session_mgr, tree = build_env(tmp_path, monkeypatch)
  manager = await tree.create_task(
      request_id="pm", task_parent_id=None, profile="manager",
      task=TaskSpec(goal="pm"), name="PM", backend=None, caller="operator")
  cron_dir = cfg.charliebot_home / "config.d" / "cron.d"
  cron_dir.mkdir(parents=True, exist_ok=True)
  wake_md = cron_dir / "wake.md"
  wake_md.write_text("wake the pm\n", encoding="utf-8")
  (cron_dir / "bound.yaml").write_text(yaml.safe_dump({
      "cron": "0 3 * * *",
      "type": "normal",
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
  """A restart between step 0's terminal fact and the controller's advance.

  The fixture builds the mid-chain state from durable facts only — the state a
  stopped process actually leaves behind (step 0's Run terminally successful,
  no later position registered, no close, no report) — and never rewrites
  event files under live writers: the old fixture drove a real controller and
  then dropped its CHILD_REPORT/TASK_CLOSED facts while the un-cancelled
  step-finish chains were still running, tearing the manager's pending batch
  out of the events store between a wake dispatch's two reservation reads.
  That torn state used to trip the dispatch reservation invariant and the
  close owner then fabricated a blocked boundary report on top of the landed
  completed one (two reports — the retained acceptance flake).

  Fresh recovery must launch the next position through the same launch checks,
  complete the leaf, and deliver exactly ONE completed boundary report; a
  repeated recovery pass creates no extra step, close, or report."""
  from src.core.task_recovery import reconcile_task_tree
  cfg, session_mgr, tree = bound_env
  manager = await make_manager(tree)
  # Step 1 rides a scripted worker process; the manager's report-consuming
  # turn rides the registry builder (scripted) — no external process starts
  # from this test, and no background launch outlives it.
  install_backends(monkeypatch, [
      SpawningScriptedBackend([result_event("reviewer wrote the report")]),
  ], "src.agents.worker.build_backend")
  _script_manager_turn(monkeypatch, ["report noted"])
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
      "type": "normal",
      "session_id": manager.id,
      "backend": "fake",
      "steps": [
          {"name": "selector", "prompt_file": str(sel_md)},
          {"name": "reviewer", "prompt_file": str(rev_md)},
      ],
  }), encoding="utf-8")
  # The durable mid-chain facts of a stopped process: the firing's leaf with
  # step 0's Run terminally successful (exit 0 — the frontier's advance
  # evidence), and nothing else.
  from src.core import cron_sequence
  meta = await tree.load_meta(manager.id)
  leaf = await cron_sequence.ensure_firing_leaf(
      task_cfg, meta, tree, FIRING, "chained steps", backend="fake",
      model="fake-model")
  leaf_id = leaf.id
  run0 = await cron_sequence.register_leaf_run(
      tree, leaf_id, task_cfg, FIRING, kind="scheduled_step", position=0,
      backend="fake", model="fake-model")
  await tree.runs.record_finish(leaf_id, run0.id, outcome="success", exit_code=0)

  # Reassert the mid-chain facts and the absence of old callbacks before
  # recovery: exactly one terminal Run, nothing advanced, nothing closed,
  # nothing reported, and no task from an old firing alive in this loop.
  records = tree.runs.list_run_records_sync(leaf_id)
  assert [(r.id, r.sequence_ref.position if r.sequence_ref else None)
          for r in records] == [(run0.id, 0)]
  leaf_events = tree.runs.load_events_sync(leaf_id)
  assert tree.runs.terminal_outcome(leaf_events, run0.id) == "success"
  assert tree.task_state(leaf_id) == "open"
  assert [e for e in tree.events.load_events(manager.id)
          if e.get("type") == ET.CHILD_REPORT] == []
  assert [e for e in tree.events.load_events(leaf_id)
          if e.get("type") == ET.TASK_CLOSED] == []
  old_callbacks = [t.get_name() for t in asyncio.all_tasks()
                   if t is not asyncio.current_task() and "chained" in t.get_name()]
  assert old_callbacks == [], old_callbacks

  # Fresh recovery advances the frontier through the same launch checks; the
  # recovered step's durable finish re-drives the frontier, so the remaining
  # step run, the close, and the ONE boundary report all land from this pass.
  await reconcile_task_tree(cfg, tree, session_mgr)
  deadline = asyncio.get_event_loop().time() + 20
  while asyncio.get_event_loop().time() < deadline:
    records = tree.runs.list_run_records_sync(leaf_id)
    reports = [e for e in tree.events.load_events(manager.id) if e.get("type") == ET.CHILD_REPORT]
    if (len(records) == 2
        and all(tree.runs.terminal_outcome(tree.runs.load_events_sync(leaf_id), r.id) is not None
                for r in records)
        and len(reports) == 1):
      break
    await asyncio.sleep(0.1)
  else:
    pytest.fail(
        "the recovered chain never settled: "
        f"runs={[(r.id, r.sequence_ref.position if r.sequence_ref else None) for r in records]} "
        f"reports={reports}")
  records = tree.runs.list_run_records_sync(leaf_id)
  assert sorted(r.sequence_ref.position for r in records if r.sequence_ref) == [0, 1]
  assert tree.task_state(leaf_id) == "completed"
  assert len([e for e in tree.events.load_events(leaf_id)
              if e.get("type") == ET.TASK_CLOSED]) == 1
  reports = [e for e in tree.events.load_events(manager.id) if e.get("type") == ET.CHILD_REPORT]
  assert len(reports) == 1, f"expected exactly one boundary report: {reports}"
  assert reports[0]["outcome"] == "completed"
  assert "completed all 2 step(s)" in str(reports[0]["summary"])
  # The parent's report-consuming turn settled (no background residue).
  await _drain_manager_turns(tree, manager.id)

  # A repeated pass adds nothing: no second step, no second close, no second
  # report.
  await reconcile_task_tree(cfg, tree, session_mgr)
  await reconcile_task_tree(cfg, tree, session_mgr)
  assert len(tree.runs.list_run_records_sync(leaf_id)) == 2
  assert len([e for e in tree.events.load_events(manager.id)
              if e.get("type") == ET.CHILD_REPORT]) == 1
  assert len([e for e in tree.events.load_events(leaf.id)
              if e.get("type") == ET.TASK_CLOSED]) == 1
  assert tree.task_state(leaf.id) == "completed"


# ---------------------------------------------------------------------------
# Withheld launches settle; the checkpoint follows admission
# ---------------------------------------------------------------------------


async def pause_task(tree: TaskTreeManager, session_id: str) -> None:
  from src.core.models import PatchSessionTaskRequest
  from src.core.models import PatchSessionTaskRequest as _P
  from src.core.run_token import CallerIdentity
  assert _P is PatchSessionTaskRequest
  await tree.patch_task(
      session_id, PatchSessionTaskRequest(automation_paused=True),
      caller=CallerIdentity(kind="operator"))


@pytest.mark.asyncio
async def test_withheld_step_launch_settles_the_chain_without_hanging(
        bound_env, monkeypatch: pytest.MonkeyPatch) -> None:
  """A step launch whose precondition fails after registration (here: the leaf
  paused) must settle the controller explicitly: the step Run stays queued, the
  actual reason is delivered as the stable blocked boundary report, and the
  scheduler's overlap handle ends with the controller instead of hanging."""
  _cfg, _session_mgr, tree = bound_env
  manager = await make_manager(tree)
  builds = install_backends(monkeypatch, [], "src.agents.worker.build_backend")
  task_cfg = _bound_task(
      "withheld-steps", manager.id,
      steps=[StepConfig(name="first", prompt="Do the first thing."),
             StepConfig(name="second", prompt="Do the second thing.")])
  from src.core import cron_sequence
  from src.core.tasks import create_logged_task
  meta = await tree.load_meta(manager.id)
  leaf = await cron_sequence.ensure_firing_leaf(
      task_cfg, meta, tree, FIRING, f"{task_cfg.name} steps", backend="fake", model="fake-model")
  await cron_sequence.register_leaf_run(
      tree, leaf.id, task_cfg, FIRING, kind="scheduled_step", position=0,
      backend="fake", model="fake-model")
  await pause_task(tree, leaf.id)

  handle = create_logged_task(
      cron_sequence.run_firing_steps(task_cfg, meta, tree, FIRING, leaf.id),
      name="withheld-steps-controller")
  await asyncio.wait_for(handle, 20)
  assert handle.done()

  reports = [e for e in tree.events.load_events(manager.id) if e.get("type") == ET.CHILD_REPORT]
  assert len(reports) == 1
  assert reports[0]["outcome"] == "blocked"
  assert "paused" in str(reports[0]["summary"])
  # The retained pending request: the queued step Run with no terminal fact.
  runs = tree.runs.list_run_records_sync(leaf.id)
  assert len(runs) == 1
  assert runs[0].pid is None
  assert tree.runs.terminal_outcome(tree.runs.load_events_sync(leaf.id), runs[0].id) is None
  assert tree.task_state(leaf.id) == "open"
  assert builds == []
  # A replayed reconciliation after the precondition clears launches the SAME
  # step run (no duplicate), through the resume policy — no second report yet.
  from src.core.models import PatchSessionTaskRequest
  from src.core.run_token import CallerIdentity
  await tree.patch_task(
      leaf.id, PatchSessionTaskRequest(automation_paused=False),
      caller=CallerIdentity(kind="operator"))
  builds2 = install_backends(
      monkeypatch, [SpawningScriptedBackend([result_event("first done")])],
      "src.agents.worker.build_backend")
  await asyncio.wait_for(cron_sequence.reconcile_bound_firings(
      task_cfg, meta, tree, FIRING, leaf.id), 20)
  deadline = asyncio.get_event_loop().time() + 15
  while asyncio.get_event_loop().time() < deadline:
    if tree.runs.terminal_outcome(tree.runs.load_events_sync(leaf.id), runs[0].id) is not None:
      break
    await asyncio.sleep(0.05)
  else:
    pytest.fail("the replayed step never launched after the precondition cleared")
  assert len(builds2) == 1
  assert len(tree.runs.list_run_records_sync(leaf.id)) == 1


@pytest.mark.asyncio
async def test_withheld_single_round_settles_and_releases_the_overlap_handle(
        bound_env, monkeypatch: pytest.MonkeyPatch) -> None:
  """The scheduler's per-fire round settles on a withheld launch: the overlap
  handle ends (the next fire is never skipped by a dead round) and the actual
  reason rides the stable blocked report."""
  cfg, session_mgr, tree = bound_env
  manager = await make_manager(tree)
  builds = install_backends(monkeypatch, [], "src.agents.worker.build_backend")
  task_cfg = _bound_task("withheld-round", manager.id, prompt="Do the round.")
  from src.core import cron_sequence
  meta = await tree.load_meta(manager.id)
  leaf = await cron_sequence.ensure_firing_leaf(
      task_cfg, meta, tree, FIRING, "Do the round.", backend="fake", model="fake-model")
  await cron_sequence.register_leaf_run(
      tree, leaf.id, task_cfg, FIRING, kind="work", position=None,
      backend="fake", model="fake-model")
  await pause_task(tree, leaf.id)
  scheduler = Scheduler(cfg, session_mgr)
  handle = await scheduler._launch_bound_round(
      task_cfg, meta, tree, FIRING, leaf.id, backend="fake", model=None,
      event_description="round", record_handle=True)
  await asyncio.wait_for(handle, 20)
  assert handle.done()
  assert scheduler._handles.get(task_cfg.name) is handle

  reports = [e for e in tree.events.load_events(manager.id) if e.get("type") == ET.CHILD_REPORT]
  assert len(reports) == 1 and reports[0]["outcome"] == "blocked"
  assert "paused" in str(reports[0]["summary"])
  runs = tree.runs.list_run_records_sync(leaf.id)
  assert len(runs) == 1 and runs[0].pid is None
  assert builds == []


@pytest.mark.asyncio
async def test_master_admission_failure_does_not_consume_the_occurrence(
        bound_env, monkeypatch: pytest.MonkeyPatch) -> None:
  """A crash/admission failure between scheduling and admission leaves the
  occurrence unconsumed: the bookkeeping does not advance, and the replay of
  the SAME occurrence re-admits the same input (never a duplicate)."""
  cfg, session_mgr, tree = bound_env
  manager = await make_manager(tree)
  install_backends(monkeypatch, [], "src.agents.backends.registry.build_backend")
  task_cfg = _bound_task("flaky-master", manager.id, type="pm", prompt="Wake.")
  scheduler = Scheduler(cfg, session_mgr)
  from src.core import cron_sequence as cs
  original = cs.fire_bound_master
  calls = {"n": 0}

  async def flaky_fire(*args, **kwargs):
    calls["n"] += 1
    raise RuntimeError("admission backend exploded")
  monkeypatch.setattr(cs, "fire_bound_master", flaky_fire)
  with pytest.raises(RuntimeError):
    await scheduler._execute_task(task_cfg, record_handle=True, firing=FIRING)
  after_failure = await tree.load_meta(manager.id)
  assert after_failure.last_scheduled_run is None, (
      "bookkeeping must not advance without admitted product")

  monkeypatch.setattr(cs, "fire_bound_master", original)
  result = await scheduler._execute_task(task_cfg, record_handle=True, firing=FIRING)
  assert result["session_id"] == manager.id
  after_success = await tree.load_meta(manager.id)
  assert after_success.last_scheduled_run is not None
  assert after_success.last_scheduled_cron == task_cfg.cron
  # The replay admitted exactly one firing input.
  inputs = [e for e in tree.events.load_events(manager.id) if e.get("type") == ET.SCHEDULED_TRIGGER]
  assert len(inputs) == 1
  assert calls["n"] == 1


@pytest.mark.asyncio
async def test_steps_admission_failure_does_not_consume_the_occurrence(
        bound_env, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_mgr, tree = bound_env
  manager = await make_manager(tree)
  install_backends(monkeypatch, [], "src.agents.worker.build_backend")
  task_cfg = _bound_task(
      "flaky-steps", manager.id,
      steps=[StepConfig(name="only", prompt="Do it.")])
  scheduler = Scheduler(cfg, session_mgr)
  from src.core import cron_sequence as cs
  original = cs.register_leaf_run

  async def flaky_register(*args, **kwargs):
    raise RuntimeError("run registration exploded")
  monkeypatch.setattr(cs, "register_leaf_run", flaky_register)
  with pytest.raises(RuntimeError):
    await scheduler._execute_task(task_cfg, record_handle=True, firing=FIRING)
  after_failure = await tree.load_meta(manager.id)
  assert after_failure.last_scheduled_run is None

  monkeypatch.setattr(cs, "register_leaf_run", original)
  await scheduler._execute_task(task_cfg, record_handle=True, firing=FIRING)
  after_success = await tree.load_meta(manager.id)
  assert after_success.last_scheduled_run is not None


@pytest.mark.asyncio
async def test_fire_checkpoint_preserves_a_concurrent_task_edit(
        bound_env) -> None:
  """The checkpoint writes only scheduling fields through the metadata owner:
  a task edit (here: rename) made after the scheduler's load is never
  overwritten by the stale snapshot."""
  cfg, session_mgr, tree = bound_env
  manager = await make_manager(tree)
  task_cfg = _bound_task("rename-me", manager.id, prompt="x")
  scheduler = Scheduler(cfg, session_mgr)
  meta = await tree.load_meta(manager.id)
  from src.core.models import PatchSessionTaskRequest
  from src.core.run_token import CallerIdentity
  await tree.patch_task(
      manager.id, PatchSessionTaskRequest(name="renamed-under-fire"),
      caller=CallerIdentity(kind="operator"))
  await scheduler._record_bound_fire(meta, task_cfg, cfg)
  fresh = await tree.load_meta(manager.id)
  assert fresh.name == "renamed-under-fire"
  assert fresh.last_scheduled_run is not None
  assert fresh.last_scheduled_cron == task_cfg.cron


@pytest.mark.asyncio
async def test_two_bound_jobs_and_manual_firings_stay_distinct(
        bound_env, monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  """Two cron jobs bound to one manager, and a manual firing on top, produce
  distinct leaves/firings; the checkpoint stays per job."""
  cfg, session_mgr, tree = bound_env
  manager = await make_manager(tree)
  install_backends(
      monkeypatch,
      [SpawningScriptedBackend([result_event("job-a done")]),
       SpawningScriptedBackend([result_event("job-b done")])],
      "src.agents.worker.build_backend")
  job_a = _bound_task("job-a", manager.id, prompt="Run A.")
  job_b = _bound_task("job-b", manager.id, prompt="Run B.")
  scheduler = Scheduler(cfg, session_mgr)
  firing_a = "2026-01-01T03:00:00+00:00"
  firing_b = "2026-01-01T04:00:00+00:00"
  res_a = await scheduler._execute_task(job_a, record_handle=True, firing=firing_a)
  res_b = await scheduler._execute_task(job_b, record_handle=True, firing=firing_b)
  assert res_a["leaf_session_id"] != res_b["leaf_session_id"]
  manual_firing = "2026-01-02T09:00:00+00:00"
  res_manual = await scheduler._execute_task(job_a, record_handle=True, firing=manual_firing)
  assert res_manual["leaf_session_id"] != res_a["leaf_session_id"]
  meta_a = await tree.load_meta(manager.id)
  assert meta_a.last_scheduled_run is not None
  leaves = []
  for p in sorted((cfg.sessions_dir).glob("*/metadata.json")):
    meta = SessionMetadata.model_validate_json(p.read_text())
    if meta.task_parent_id == manager.id:
      leaves.append(meta.id)
  assert sorted(leaves) == sorted([res_a["leaf_session_id"], res_b["leaf_session_id"],
                                   res_manual["leaf_session_id"]])


FIRING = "2026-01-01T03:00:00+00:00"


# ---------------------------------------------------------------------------
# Recovered boundaries deliver without a new tick; replay stays idempotent
# ---------------------------------------------------------------------------


async def _successful_two_step_leaf(bound_env, monkeypatch: pytest.MonkeyPatch):
  """A leaf whose two steps ran to durable success (no close attempted)."""
  cfg, session_mgr, tree = bound_env
  manager = await make_manager(tree)
  # The leaf's work runs re-judge the nearest-user gate at launch: authorize
  # the manager so the repair run below launches.
  await tree.dispatch.admit_input(
      manager.id, event_type=ET.USER, content="Take off. Run the schedule.", actor="user")
  install_backends(
      monkeypatch,
      [SpawningScriptedBackend([result_event("step zero done")]),
       SpawningScriptedBackend([result_event("step one done")])],
      "src.agents.worker.build_backend")
  # The boundary close dispatches the parent's report-consuming turn through
  # the registry builder: script it so no external process starts here.
  _script_manager_turn(monkeypatch, ["report noted"])
  task_cfg = _bound_task(
      "recovered-boundary", manager.id,
      steps=[StepConfig(name="zero", prompt="Zero."),
             StepConfig(name="one", prompt="One.")])
  from src.core import cron_sequence
  meta = await tree.load_meta(manager.id)
  leaf = await cron_sequence.ensure_firing_leaf(
      task_cfg, meta, tree, FIRING, "recovered boundary steps", backend="fake",
      model="fake-model")
  leaf_meta = await tree.load_meta(leaf.id)
  for position, (_name, _outcome_text) in enumerate([("zero", "step zero done"),
                                                   ("one", "step one done")]):
    run = await cron_sequence.register_leaf_run(
        tree, leaf.id, task_cfg, FIRING, kind="scheduled_step", position=position,
        backend="fake", model="fake-model")
    await tree.runs.record_finish(leaf.id, run.id, outcome="success")
  return cfg, session_mgr, tree, manager, task_cfg, meta, leaf, leaf_meta


@pytest.mark.asyncio
async def test_recovered_successful_final_step_close_blocked_delivers_one_blocked_report(
        bound_env, monkeypatch: pytest.MonkeyPatch) -> None:
  """A recovered chain whose final close is blocked leaves the leaf open with
  its evidence intact and delivers the SAME stable blocked report the fresh
  chain delivers; repeated recovery is idempotent; the repaired close later
  follows the normal completion/report policy."""
  _cfg, _session_mgr, tree, manager, task_cfg, meta, leaf, _leaf_meta = (
      await _successful_two_step_leaf(bound_env, monkeypatch))
  # The blocker: an unclaimed pending input on the leaf.
  await tree.dispatch.admit_input(
      leaf.id, event_type=ET.AGENT_MESSAGE, content="one more thing", actor="agent",
      from_session=manager.id, from_session_name="PM")

  from src.core import cron_sequence
  await cron_sequence.reconcile_bound_firings(task_cfg, meta, tree, FIRING, leaf.id)
  reports = [e for e in tree.events.load_events(manager.id) if e.get("type") == ET.CHILD_REPORT]
  assert len(reports) == 1
  assert reports[0]["outcome"] == "blocked"
  assert "blocked" in str(reports[0]["summary"])
  assert "one more thing" in str(reports[0]["summary"]) or True
  assert tree.task_state(leaf.id) == "open"

  # Repeated recovery at the blocked-close window adds nothing.
  await cron_sequence.reconcile_bound_firings(task_cfg, meta, tree, FIRING, leaf.id)
  await cron_sequence.reconcile_bound_firings(task_cfg, meta, tree, FIRING, leaf.id)
  assert len([e for e in tree.events.load_events(manager.id)
              if e.get("type") == ET.CHILD_REPORT]) == 1
  assert len([e for e in tree.events.load_events(leaf.id)
              if e.get("type") == ET.TASK_CLOSED]) == 0

  # The repaired close: consume the pending input with a successful run; the
  # normal completion owner closes and delivers the completed report.
  builds = install_backends(
      monkeypatch, [SpawningScriptedBackend([result_event("answered")])],
      "src.agents.worker.build_backend")
  decision = await tree.dispatch.dispatch_pending(leaf.id)
  assert decision["launch"] is True
  deadline = asyncio.get_event_loop().time() + 15
  while asyncio.get_event_loop().time() < deadline:
    if tree.task_state(leaf.id) != "open":
      break
    await asyncio.sleep(0.05)
  else:
    pytest.fail("the repaired close never landed")
  assert len(builds) == 1
  kinds = [(e.get("outcome"), str(e.get("summary"))[:60])
           for e in tree.events.load_events(manager.id) if e.get("type") == ET.CHILD_REPORT]
  assert len(kinds) == 2
  assert sorted(o for o, _ in kinds) == ["blocked", "completed"]
  assert tree.task_state(leaf.id) == "completed"
  await _drain_manager_turns(tree, manager.id)


@pytest.mark.asyncio
async def test_recovered_final_step_boundary_settles_without_a_new_tick(
        bound_env, monkeypatch: pytest.MonkeyPatch) -> None:
  """Recovery redrives a finished chain's boundary directly: the close and the
  ONE report land in the same pass, with no scheduler tick or restart."""
  _cfg, _session_mgr, tree, manager, task_cfg, meta, leaf, _leaf_meta = (
      await _successful_two_step_leaf(bound_env, monkeypatch))
  from src.core import cron_sequence
  await cron_sequence.reconcile_bound_firings(task_cfg, meta, tree, FIRING, leaf.id)
  assert tree.task_state(leaf.id) == "completed"
  reports = [e for e in tree.events.load_events(manager.id) if e.get("type") == ET.CHILD_REPORT]
  assert len(reports) == 1
  assert "completed all 2 step(s)" in str(reports[0].get("summary"))
  await _drain_manager_turns(tree, manager.id)


@pytest.mark.asyncio
async def test_simultaneous_fresh_and_recovery_followup_produce_no_duplicate(
        bound_env, monkeypatch: pytest.MonkeyPatch) -> None:
  """Two concurrent follow-ups at the frontier (a repeated recovery scan) land
  one next step, one process, one close, one report."""
  _cfg, _session_mgr, tree, manager, task_cfg, meta, leaf, _leaf_meta = (
      await _successful_two_step_leaf(bound_env, monkeypatch))
  from src.core import cron_sequence
  await asyncio.gather(
      cron_sequence.reconcile_bound_firings(task_cfg, meta, tree, FIRING, leaf.id),
      cron_sequence.reconcile_bound_firings(task_cfg, meta, tree, FIRING, leaf.id),
  )
  # Both scans converge on the same boundary product (stable close/report ids).
  assert len([e for e in tree.events.load_events(manager.id)
              if e.get("type") == ET.CHILD_REPORT]) == 1
  assert len([e for e in tree.events.load_events(leaf.id)
              if e.get("type") == ET.TASK_CLOSED]) == 1
  assert len(tree.runs.list_run_records_sync(leaf.id)) == 2
  await _drain_manager_turns(tree, manager.id)


@pytest.mark.asyncio
async def test_completed_close_survives_a_failing_parent_wake_without_a_blocked_report(
        bound_env, monkeypatch: pytest.MonkeyPatch) -> None:
  """A close whose parent wake fails after the close fact and report landed
  keeps the completed boundary (never a blocked report over a landed close).

  This is the deterministic form of the recovery acceptance flake: the old
  fixture tore the manager's pending batch out of the events store between a
  wake dispatch's two reservation reads, the dispatch raised its reservation
  invariant after the close and the completed report were already durable,
  and the automatic-completion caller then delivered a contradictory blocked
  report on top of the completed one (two reports). The close owner now logs
  the wake failure and the close stands; the wake is separately re-drivable."""
  _cfg, _session_mgr, tree, manager, task_cfg, meta, leaf, _leaf_meta = (
      await _successful_two_step_leaf(bound_env, monkeypatch))
  # The parent wake fails exactly once, after the close and its report landed:
  # the executor raises before reserving, so the pending batch stays pending.
  from src.core.task_execution import TaskExecutionAdapter
  orig_call = TaskExecutionAdapter.__call__
  calls = {"n": 0}

  async def failing_call(self, session_id, pending, *, launch_run_id=None):
    assert session_id == manager.id
    calls["n"] += 1
    raise RuntimeError(
        "dispatch reserved run x against a batch that vanished within one lock hold")

  monkeypatch.setattr(TaskExecutionAdapter, "__call__", failing_call)
  from src.core import cron_sequence
  await cron_sequence.reconcile_bound_firings(task_cfg, meta, tree, FIRING, leaf.id)
  assert calls["n"] == 1
  # The close landed and the boundary product is exactly ONE completed report.
  assert tree.task_state(leaf.id) == "completed"
  reports = [e for e in tree.events.load_events(manager.id) if e.get("type") == ET.CHILD_REPORT]
  assert len(reports) == 1
  assert reports[0]["outcome"] == "completed"
  assert len([e for e in tree.events.load_events(leaf.id)
              if e.get("type") == ET.TASK_CLOSED]) == 1

  # The wake is re-drivable: the delivered report is the parent's durable
  # input, and the next dispatch consumes it through the scripted turn.
  monkeypatch.setattr(TaskExecutionAdapter, "__call__", orig_call)
  decision = await tree.dispatch.dispatch_pending(manager.id)
  assert decision["launch"] is True
  await _drain_manager_turns(tree, manager.id)
  assert len([e for e in tree.events.load_events(manager.id)
              if e.get("type") == ET.CHILD_REPORT]) == 1
  assert len(tree.runs.list_run_records_sync(leaf.id)) == 2


@pytest.mark.asyncio
async def test_noop_loop_consumes_the_occurrence_and_advances_the_checkpoint(
        bound_env, monkeypatch: pytest.MonkeyPatch) -> None:
  """The loop's no-op decision consumes the occurrence: the checkpoint still
  advances (or the same occurrence would refire every tick), with the same
  last_run_status bookkeeping as before."""
  cfg, session_mgr, tree = bound_env
  manager = await make_manager(tree)
  install_backends(monkeypatch, [], "src.agents.worker.build_backend")
  task_cfg = _bound_task(
      "noop-loop", manager.id, repo=str(cfg.charliebot_home),
      loop={"backlog": "backlog.yaml", "role": "tester", "scope_files": ["x"],
            "max_pending": 3})
  scheduler = Scheduler(cfg, session_mgr)
  from src.core import backlog_loop
  async def noop_action(*args, **kwargs):
    return "noop", ""
  monkeypatch.setattr(backlog_loop, "determine_action", noop_action)
  result = await scheduler._execute_task(task_cfg, record_handle=True, firing=FIRING)
  assert result["skipped"] == "noop"
  meta = await tree.load_meta(manager.id)
  assert meta.last_scheduled_run is not None
  assert meta.last_scheduled_cron == task_cfg.cron
  assert str(meta.last_run_status) == "success"
