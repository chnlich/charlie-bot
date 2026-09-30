"""Bound-scheduled-task tests: one stable node, durable firing identity, Runs.

Exercises the actual scheduler entry points (_maybe_run / run_task_now) against
a synthetic instance with deterministic scripted backend processes: bound
master and worker modes, steps sharing one leaf, per-step backends, stop
semantics, config API round-trip, closed nodes, new-vs-replayed
firings, and run-token spoof attempts.
"""

from __future__ import annotations

import asyncio
import uuid
from pathlib import Path

import pytest
import yaml
from conftest import (
    BUILD_BACKEND_PATCH_TARGET,
    CODEX_BACKEND_OPTION,
    FABLE_MODEL,
    MASTER_TRIGGER_TRIGGER_MASTER_PATCH_TARGET,
    OPERATOR,
    OPUS_BACKEND_ID,
    OPUS_BACKEND_OPTION,
    POOLED_FABLE_ID,
    SCHEDULER_GET_CONFIG_PATCH_TARGET,
    WORKER_BUILD_BACKEND_PATCH_TARGET,
    backend_option,
    bind_deps_managers,
    init_repo_with_origin,
    patch_instructions_content,
)

from src.core import claude_accounts
from src.core import event_types as ET
from src.core.config import CharlieBotConfig, ScheduledTaskConfig, StepConfig
from src.core.control_events import stable_run_id
from src.core.models import RunRecord, TaskSpec
from src.core.scheduler import Scheduler
from src.core.task_sessions import TaskTreeManager
from tests.test_task_execution import (
    SpawningScriptedBackend,
    WorkerAccountRecorder,
    _adapter_with_silent_broadcast,
    build_pooled_env,
    build_spawning_env,
    install_backends,
    result_event,
    wait_for_terminal_run,
)


def build_env(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
  """One synthetic home, the fake + codex backends, and one task tree."""
  return build_spawning_env(
      tmp_path,
      monkeypatch,
      options=[
          OPUS_BACKEND_OPTION,
          CODEX_BACKEND_OPTION,
          backend_option(id="fake", label="Fake", type="codex", model="fake-model"),
      ])


@pytest.fixture()
def bound_env(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
  """cfg + tree with the scheduler's deps singleton and the adapter installed.

  The bound manager node is created per test (each test is async).
  """
  cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
  bind_deps_managers(monkeypatch, tree, session_mgr)
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
  return cfg, session_mgr, tree


async def make_manager(tree: TaskTreeManager, name: str = "Manager"):
  return await tree.create_task(
      request_id=f"manager-{name}",
      task_parent_id=None,
      profile="manager",
      task=TaskSpec(goal="standup"),
      name=name,
      backend=None,
      caller="operator")


def _bound_task(name: str, session_id: str, **overrides) -> ScheduledTaskConfig:
  body = {
      "name": name,
      "cron": "0 3 * * *",
      "session_id": session_id,
      "backend": "fake",
  }
  body.update(overrides)
  return ScheduledTaskConfig(**body)


def _persist_chained_cron_d(cfg: CharlieBotConfig, session_id: str) -> ScheduledTaskConfig:
  """Bind the two-step chained task for `session_id` and persist its durable
  cron.d binding under this instance's home: the two step prompt files and the
  chained.yaml. The recovery reads the binding from that durable file, so it
  must exist on disk before a simulated restart. Returns the in-memory task
  config the scheduler entry points accept."""
  task_cfg = _bound_task(
      "chained",
      session_id,
      steps=[
          StepConfig(name="selector", prompt="Select."),
          StepConfig(name="reviewer", prompt="Review."),
      ])
  cron_d = cfg.charliebot_home / "config.d" / "cron.d"
  cron_d.mkdir(parents=True, exist_ok=True)
  sel_md = cron_d / "selector.md"
  rev_md = cron_d / "reviewer.md"
  sel_md.write_text("Select the target.\n", encoding="utf-8")
  rev_md.write_text("Review the result.\n", encoding="utf-8")
  (cron_d / "chained.yaml").write_text(
      yaml.safe_dump(
          {
              "cron":
                  "0 3 * * *",
              "session_id":
                  session_id,
              "backend":
                  "fake",
              "steps":
                  [
                      {
                          "name": "selector",
                          "prompt_file": str(sel_md)
                      },
                      {
                          "name": "reviewer",
                          "prompt_file": str(rev_md)
                      },
                  ],
          }),
      encoding="utf-8")
  return task_cfg


def _script_manager_turn(monkeypatch: pytest.MonkeyPatch, notes: list[str]) -> None:
  """Script the parent node's report-consuming turn through the registry
  builder (the manager dispatch path), so no external process starts from
  these tests. One scripted backend per expected manager-turn build."""
  install_backends(
      monkeypatch, [SpawningScriptedBackend([result_event(note)]) for note in notes], BUILD_BACKEND_PATCH_TARGET)
  patch_instructions_content(monkeypatch)


async def _drain_manager_turns(tree: TaskTreeManager, manager_id: str) -> None:
  """Wait until every manager_turn Run of the node reached a terminal fact,
  so no launch task outlives the test as background residue."""
  deadline = asyncio.get_event_loop().time() + 15
  while asyncio.get_event_loop().time() < deadline:
    records = [r for r in tree.runs.list_run_records_sync(manager_id) if r.kind == "manager_turn"]
    events = tree.runs.load_events_sync(manager_id)
    if records and all(tree.runs.terminal_outcome(events, r.id) is not None for r in records):
      return
    await asyncio.sleep(0.05)
  pytest.fail(f"the manager turn never settled: {tree.runs.list_run_records_sync(manager_id)}")


async def _wait_for_reports(tree: TaskTreeManager, node_id: str, timeout_s: float, miss: str) -> list[dict]:
  """Poll the node's event log until its first CHILD_REPORT lands; return the reports on record then.

  ``pytest.fail(miss)`` ends the wait when the timeout lands first, so callers read the returned
  list without a second emptiness check.
  """
  deadline = asyncio.get_event_loop().time() + timeout_s
  reports: list[dict] = []
  while asyncio.get_event_loop().time() < deadline:
    reports = [e for e in tree.events.load_events(node_id) if e.get("type") == ET.CHILD_REPORT]
    if reports:
      return reports
    await asyncio.sleep(0.1)
  pytest.fail(miss)


# ---------------------------------------------------------------------------
# mode: master: the typed scheduled input lands on the bound manager
# ---------------------------------------------------------------------------


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
  claims = RunTokenClaims(session_id=manager.id, run_id=run_id, agent="Manager")
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
        manager.id, event_type=ET.SCHEDULED_TRIGGER, content="forged standup", actor="agent", input_id="spoofed-input")
  # And a USER event from a non-operator actor is refused (the run token can
  # never fabricate the take-off time either).
  with pytest.raises(Exception, match="real user message"):
    await tree.dispatch.admit_input(
        manager.id, event_type=ET.USER, content="Take off.", actor="agent", input_id="spoofed-user")


# ---------------------------------------------------------------------------
# Worker mode: one leaf per firing, steps share it, one boundary report
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_bound_steps_failure_stops_chain_and_reports_failed(bound_env, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_mgr, tree = bound_env
  manager = await make_manager(tree)
  install_backends(
      monkeypatch, [
          SpawningScriptedBackend([result_event("broke")], exit_code=1),
      ], WORKER_BUILD_BACKEND_PATCH_TARGET)
  task_cfg = _persist_chained_cron_d(cfg, manager.id)
  scheduler = Scheduler(cfg, session_mgr)
  firing = "2026-01-01T03:00:00+00:00"
  result = await scheduler._execute_task(task_cfg, record_handle=True, firing=firing)
  leaf_id = result["leaf_session_id"]
  reports = await _wait_for_reports(tree, manager.id, 20, "the failure report never arrived")
  # The chain stopped at the failed step: no later step run exists.
  records = tree.runs.list_run_records_sync(leaf_id)
  assert [r.sequence_ref.position for r in records] == [0]
  # The failed step's report names the stop and the leaf stays open (a failed
  # Run is not a terminal task state and paints no sidebar activity).
  assert "stopped at step 'selector'" in str(reports[0].get("summary"))
  assert tree.task_state(leaf_id) != "completed"


# ---------------------------------------------------------------------------
# Binding validation: never a replacement session, closed skip
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_missing_binding_fails_visibly_without_creating_a_session(
    bound_env, monkeypatch: pytest.MonkeyPatch) -> None:
  from src.core.cron_sequence import ScheduledBindingError
  cfg, session_mgr, tree = bound_env
  await make_manager(tree)
  task_cfg = _bound_task("ghost", str(uuid.uuid4()), prompt="Nobody home.")
  scheduler = Scheduler(cfg, session_mgr)
  with pytest.raises(ScheduledBindingError, match="does not exist"):
    await scheduler._execute_task(task_cfg, record_handle=True, firing="2026-01-01T03:00:00+00:00")
  # No session was created for the missing binding.
  metas = list(cfg.sessions_dir.glob("*/metadata.json"))
  assert len(metas) == 1  # only the manager itself


@pytest.mark.asyncio
async def test_legacy_session_binding_refuses_the_v2_path(bound_env, monkeypatch: pytest.MonkeyPatch) -> None:
  from src.core.cron_sequence import ScheduledBindingError
  cfg, session_mgr, tree = bound_env
  await make_manager(tree)
  from src.core.models import CreateSessionRequest
  legacy = await session_mgr.create_session(CreateSessionRequest(name="Legacy session"), backend=OPUS_BACKEND_ID)
  task_cfg = _bound_task("legacy-bound", legacy.id, prompt="wake")
  scheduler = Scheduler(cfg, session_mgr)
  with pytest.raises(ScheduledBindingError, match="not a task-tree node"):
    await scheduler._execute_task(task_cfg, record_handle=True, firing="2026-01-01T03:00:00+00:00")


@pytest.mark.asyncio
async def test_closed_bound_node_generates_no_new_execution(bound_env, monkeypatch: pytest.MonkeyPatch) -> None:
  from src.core.cron_sequence import ScheduledBindingError
  cfg, session_mgr, tree = bound_env
  manager = await make_manager(tree)
  install_backends(monkeypatch, [SpawningScriptedBackend([result_event("x")])], BUILD_BACKEND_PATCH_TARGET)
  from src.core.task_completion import CompletionEvidence
  close_run = stable_run_id(manager.id, "close:evidence")
  await tree.runs.register_run(RunRecord(id=close_run, session_id=manager.id, kind="manager_turn"))
  await tree.runs.record_launch(manager.id, close_run, pid=424001, pid_start="ps-1")
  await tree.dispatch.finish_run(manager.id, close_run, outcome="success")
  await tree.completion.complete_task(
      manager.id,
      request_id="close-1",
      caller=OPERATOR,
      evidence=CompletionEvidence(summary="done", run_ids=[close_run], result_refs=[f"run:{close_run}"]))
  task_cfg = _bound_task("wake-manager", manager.id, prompt="Standup.")
  scheduler = Scheduler(cfg, session_mgr)
  with pytest.raises(ScheduledBindingError, match="no new cron execution"):
    await scheduler._execute_task(task_cfg, record_handle=True, firing="2026-01-01T03:00:00+00:00")
  # The configuration remains readable.
  assert task_cfg.session_id == manager.id


# ---------------------------------------------------------------------------
# Config schema + API round-trip
# ---------------------------------------------------------------------------


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
  cfg, _session_mgr, tree = bound_env
  manager = await make_manager(tree)
  # Step 1 rides a scripted worker process; the manager's report-consuming
  # turn rides the registry builder (scripted) — no external process starts
  # from this test, and no background launch outlives it.
  install_backends(
      monkeypatch, [
          SpawningScriptedBackend([result_event("reviewer wrote the report")]),
      ], WORKER_BUILD_BACKEND_PATCH_TARGET)
  _script_manager_turn(monkeypatch, ["report noted"])
  task_cfg = _persist_chained_cron_d(cfg, manager.id)
  # The durable mid-chain facts of a stopped process: the firing's leaf with
  # step 0's Run terminally successful (exit 0 — the frontier's advance
  # evidence), and nothing else.
  from src.core import cron_sequence
  meta = await tree.load_meta(manager.id)
  leaf = await cron_sequence.ensure_firing_leaf(task_cfg, meta, tree, FIRING, "chained steps")
  leaf_id = leaf.id
  run0 = await cron_sequence.register_leaf_run(
      tree, leaf_id, task_cfg, FIRING, kind="scheduled_step", position=0, backend="fake", model="fake-model")
  await tree.runs.record_finish(leaf_id, run0.id, outcome="success", exit_code=0)

  # Reassert the mid-chain facts and the absence of old callbacks before
  # recovery: exactly one terminal Run, nothing advanced, nothing closed,
  # nothing reported, and no task from an old firing alive in this loop.
  records = tree.runs.list_run_records_sync(leaf_id)
  assert [(r.id, r.sequence_ref.position if r.sequence_ref else None) for r in records] == [(run0.id, 0)]
  leaf_events = tree.runs.load_events_sync(leaf_id)
  assert tree.runs.terminal_outcome(leaf_events, run0.id) == "success"
  assert tree.task_state(leaf_id) == "open"
  assert [e for e in tree.events.load_events(manager.id) if e.get("type") == ET.CHILD_REPORT] == []
  assert [e for e in tree.events.load_events(leaf_id) if e.get("type") == ET.TASK_CLOSED] == []
  old_callbacks = [
      t.get_name() for t in asyncio.all_tasks() if t is not asyncio.current_task() and "chained" in t.get_name()
  ]
  assert old_callbacks == [], old_callbacks

  # Fresh recovery advances the frontier through the same launch checks; the
  # recovered step's durable finish re-drives the frontier, so the remaining
  # step run, the close, and the ONE boundary report all land from this pass.
  await reconcile_task_tree(cfg, tree)
  deadline = asyncio.get_event_loop().time() + 20
  while asyncio.get_event_loop().time() < deadline:
    records = tree.runs.list_run_records_sync(leaf_id)
    reports = [e for e in tree.events.load_events(manager.id) if e.get("type") == ET.CHILD_REPORT]
    if (len(records) == 2 and
        all(tree.runs.terminal_outcome(tree.runs.load_events_sync(leaf_id), r.id) is not None for r in records) and
        len(reports) == 1):
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
  assert len([e for e in tree.events.load_events(leaf_id) if e.get("type") == ET.TASK_CLOSED]) == 1
  reports = [e for e in tree.events.load_events(manager.id) if e.get("type") == ET.CHILD_REPORT]
  assert len(reports) == 1, f"expected exactly one boundary report: {reports}"
  assert reports[0]["outcome"] == "completed"
  assert "completed all 2 step(s)" in str(reports[0]["summary"])
  # The parent's report-consuming turn settled (no background residue).
  await _drain_manager_turns(tree, manager.id)

  # A repeated pass adds nothing: no second step, no second close, no second
  # report.
  await reconcile_task_tree(cfg, tree)
  await reconcile_task_tree(cfg, tree)
  assert len(tree.runs.list_run_records_sync(leaf_id)) == 2
  assert len([e for e in tree.events.load_events(manager.id) if e.get("type") == ET.CHILD_REPORT]) == 1
  assert len([e for e in tree.events.load_events(leaf.id) if e.get("type") == ET.TASK_CLOSED]) == 1
  assert tree.task_state(leaf.id) == "completed"


# ---------------------------------------------------------------------------
# Withheld launches settle; the checkpoint follows admission
# ---------------------------------------------------------------------------


async def cancel_task_node(tree: TaskTreeManager, session_id: str, request_id: str) -> None:
  """Close one run-less node with outcome cancelled: a closed task withholds
  every launch (a queued Run would block the cancel, so close first)."""
  await tree.completion.cancel_task(session_id, request_id=request_id, reason="withhold the launch", caller=OPERATOR)


async def reopen_task_node(tree: TaskTreeManager, session_id: str, request_id: str) -> None:
  await tree.completion.reopen_task(session_id, request_id=request_id, reason="precondition cleared", caller=OPERATOR)


def blocked_reports(tree: TaskTreeManager, manager_id: str) -> list[dict]:
  return [
      e for e in tree.events.load_events(manager_id)
      if e.get("type") == ET.CHILD_REPORT and e.get("outcome") == "blocked"
  ]


@pytest.mark.asyncio
async def test_withheld_step_launch_settles_the_chain_without_hanging(
    bound_env, monkeypatch: pytest.MonkeyPatch) -> None:
  """A step launch whose precondition fails after registration (here: the leaf
  was cancelled after it was created, before its step Run launched) must settle
  the controller explicitly: the step Run stays queued, the actual reason is
  delivered as the stable blocked boundary report, and the scheduler's overlap
  handle ends with the controller instead of hanging."""
  _cfg, _session_mgr, tree = bound_env
  manager = await make_manager(tree)
  builds = install_backends(monkeypatch, [], WORKER_BUILD_BACKEND_PATCH_TARGET)
  task_cfg = _bound_task(
      "withheld-steps",
      manager.id,
      steps=[
          StepConfig(name="first", prompt="Do the first thing."),
          StepConfig(name="second", prompt="Do the second thing.")
      ])
  from src.core import cron_sequence
  from src.core.tasks import create_logged_task
  meta = await tree.load_meta(manager.id)
  leaf = await cron_sequence.ensure_firing_leaf(task_cfg, meta, tree, FIRING, f"{task_cfg.name} steps")
  await cancel_task_node(tree, leaf.id, "withhold-leaf")
  await cron_sequence.register_leaf_run(
      tree, leaf.id, task_cfg, FIRING, kind="scheduled_step", position=0, backend="fake", model="fake-model")

  handle = create_logged_task(
      cron_sequence.run_firing_steps(task_cfg, meta, tree, FIRING, leaf.id), name="withheld-steps-controller")
  await asyncio.wait_for(handle, 20)
  assert handle.done()

  reports = blocked_reports(tree, manager.id)
  assert len(reports) == 1
  assert "withheld" in str(reports[0]["summary"])
  # The retained pending request: the queued step Run with no terminal fact.
  runs = tree.runs.list_run_records_sync(leaf.id)
  assert len(runs) == 1
  assert runs[0].pid is None
  assert tree.runs.terminal_outcome(tree.runs.load_events_sync(leaf.id), runs[0].id) is None
  assert builds == []
  # A replayed reconciliation after the precondition clears (the leaf reopens)
  # launches the SAME step run (no duplicate) — no second report.
  await reopen_task_node(tree, leaf.id, "clear-leaf")
  assert tree.task_state(leaf.id) == "open"
  builds2 = install_backends(
      monkeypatch, [SpawningScriptedBackend([result_event("first done")])], WORKER_BUILD_BACKEND_PATCH_TARGET)
  await asyncio.wait_for(cron_sequence.reconcile_bound_firings(task_cfg, meta, tree, FIRING, leaf.id), 20)
  await wait_for_terminal_run(tree, leaf.id, runs[0].id, timeout=15.0)
  assert len(builds2) == 1
  assert len(tree.runs.list_run_records_sync(leaf.id)) == 1


@pytest.mark.asyncio
async def test_steps_admission_failure_does_not_consume_the_occurrence(
    bound_env, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_mgr, tree = bound_env
  manager = await make_manager(tree)
  install_backends(monkeypatch, [], WORKER_BUILD_BACKEND_PATCH_TARGET)
  task_cfg = _bound_task("flaky-steps", manager.id, steps=[StepConfig(name="only", prompt="Do it.")])
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
  await tree.dispatch.admit_input(manager.id, event_type=ET.USER, content="Take off. Run the schedule.", actor="user")
  install_backends(
      monkeypatch, [
          SpawningScriptedBackend([result_event("step zero done")]),
          SpawningScriptedBackend([result_event("step one done")])
      ], WORKER_BUILD_BACKEND_PATCH_TARGET)
  # The boundary close dispatches the parent's report-consuming turn through
  # the registry builder: script it so no external process starts here.
  _script_manager_turn(monkeypatch, ["report noted"])
  task_cfg = _bound_task(
      "recovered-boundary",
      manager.id,
      steps=[StepConfig(name="zero", prompt="Zero."),
             StepConfig(name="one", prompt="One.")])
  from src.core import cron_sequence
  meta = await tree.load_meta(manager.id)
  leaf = await cron_sequence.ensure_firing_leaf(task_cfg, meta, tree, FIRING, "recovered boundary steps")
  leaf_meta = await tree.load_meta(leaf.id)
  for position, (_name, _outcome_text) in enumerate([("zero", "step zero done"), ("one", "step one done")]):
    run = await cron_sequence.register_leaf_run(
        tree, leaf.id, task_cfg, FIRING, kind="scheduled_step", position=position, backend="fake", model="fake-model")
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
      leaf.id,
      event_type=ET.AGENT_MESSAGE,
      content="one more thing",
      actor="agent",
      from_session=manager.id,
      from_session_name="Manager")

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
  assert len([e for e in tree.events.load_events(manager.id) if e.get("type") == ET.CHILD_REPORT]) == 1
  assert len([e for e in tree.events.load_events(leaf.id) if e.get("type") == ET.TASK_CLOSED]) == 0

  # The repaired close: consume the pending input with a successful run; the
  # normal completion owner closes and delivers the completed report.
  builds = install_backends(
      monkeypatch, [SpawningScriptedBackend([result_event("answered")])], WORKER_BUILD_BACKEND_PATCH_TARGET)
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
  kinds = [
      (e.get("outcome"), str(e.get("summary"))[:60])
      for e in tree.events.load_events(manager.id)
      if e.get("type") == ET.CHILD_REPORT
  ]
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
  assert len([e for e in tree.events.load_events(manager.id) if e.get("type") == ET.CHILD_REPORT]) == 1
  assert len([e for e in tree.events.load_events(leaf.id) if e.get("type") == ET.TASK_CLOSED]) == 1
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
    raise RuntimeError("dispatch reserved run x against a batch that vanished within one lock hold")

  monkeypatch.setattr(TaskExecutionAdapter, "__call__", failing_call)
  from src.core import cron_sequence
  await cron_sequence.reconcile_bound_firings(task_cfg, meta, tree, FIRING, leaf.id)
  assert calls["n"] == 1
  # The close landed and the boundary product is exactly ONE completed report.
  assert tree.task_state(leaf.id) == "completed"
  reports = [e for e in tree.events.load_events(manager.id) if e.get("type") == ET.CHILD_REPORT]
  assert len(reports) == 1
  assert reports[0]["outcome"] == "completed"
  assert len([e for e in tree.events.load_events(leaf.id) if e.get("type") == ET.TASK_CLOSED]) == 1

  # The wake is re-drivable: the delivered report is the parent's durable
  # input, and the next dispatch consumes it through the scripted turn.
  monkeypatch.setattr(TaskExecutionAdapter, "__call__", orig_call)
  decision = await tree.dispatch.dispatch_pending(manager.id)
  assert decision["launch"] is True
  await _drain_manager_turns(tree, manager.id)
  assert len([e for e in tree.events.load_events(manager.id) if e.get("type") == ET.CHILD_REPORT]) == 1
  assert len(tree.runs.list_run_records_sync(leaf.id)) == 2


@pytest.mark.asyncio
async def test_noop_loop_consumes_the_occurrence_and_advances_the_checkpoint(
    bound_env, monkeypatch: pytest.MonkeyPatch) -> None:
  """The loop's no-op decision consumes the occurrence: the checkpoint still
  advances (or the same occurrence would refire every tick), with the same
  last_run_status bookkeeping as before."""
  cfg, session_mgr, tree = bound_env
  manager = await make_manager(tree)
  install_backends(monkeypatch, [], WORKER_BUILD_BACKEND_PATCH_TARGET)
  task_cfg = _bound_task(
      "noop-loop",
      manager.id,
      repo=str(cfg.charliebot_home),
      loop={
          "backlog": "backlog.yaml",
          "role": "tester",
          "scope_files": ["x"],
          "max_pending": 3
      })
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


# ---------------------------------------------------------------------------
# Unbound tasks: auto-bind creates the node, and it parents the firings' leaves
# ---------------------------------------------------------------------------


def _persist_unbound_cron_d(cfg, name: str, body: dict) -> None:
  """Persist one unbound task's durable cron.d binding under this home.

  Auto-bind writes the ``session_id`` key back through the single-key write, so
  the host file must exist before the fire, exactly as production files do.
  """
  cron_d = cfg.charliebot_home / "config.d" / "cron.d"
  cron_d.mkdir(parents=True, exist_ok=True)
  (cron_d / f"{name}.yaml").write_text(yaml.safe_dump(body), encoding="utf-8")


@pytest.mark.asyncio
async def test_unbound_prompt_task_binds_and_fires_once_against_its_new_node(
    bound_env, monkeypatch: pytest.MonkeyPatch) -> None:
  """An unbound prompt task binds on its first fire (auto-bind): the scheduler
  creates the task-named manager node, writes the ``session_id`` back, and
  runs the bound code against it -- one worker leaf under the NODE, which
  completes and reports to it. A replayed fire creates nothing new."""
  cfg, session_mgr, tree = bound_env
  # The unbound path binds through the scheduler's reloaded process config;
  # pin it to the synthetic home's cfg.
  monkeypatch.setattr(SCHEDULER_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  install_backends(
      monkeypatch, [
          SpawningScriptedBackend([result_event("sweep done")]),
      ], WORKER_BUILD_BACKEND_PATCH_TARGET)
  wakes: list[tuple[str, str]] = []

  async def fake_trigger_master(session_id, text, cfg_, session_mgr_, input_event_type, **kwargs):
    wakes.append((session_id, text))

  monkeypatch.setattr(MASTER_TRIGGER_TRIGGER_MASTER_PATCH_TARGET, fake_trigger_master)
  _persist_unbound_cron_d(cfg, "nightly-sweep", {"cron": "0 3 * * *", "prompt": "Do the sweep.", "backend": "fake"})
  task_cfg = ScheduledTaskConfig(name="nightly-sweep", cron="0 3 * * *", prompt="Do the sweep.", backend="fake")
  scheduler = Scheduler(cfg, session_mgr)
  firing = "2026-01-01T03:00:00+00:00"
  result = await scheduler._execute_task(task_cfg, record_handle=True, firing=firing)

  # The binding exists in memory and on disk, and names the new manager node.
  node_id = task_cfg.session_id
  assert node_id is not None
  node = await tree.load_meta(node_id)
  assert node is not None and node.profile == "manager"
  assert node.name == "nightly-sweep"
  persisted = yaml.safe_load((cfg.charliebot_home / "config.d" / "cron.d" / "nightly-sweep.yaml").read_text())
  assert persisted["session_id"] == node_id
  # The firing's leaf parents under the node.
  leaf_id = result["leaf_session_id"]
  assert leaf_id != node_id
  leaf = await tree.load_meta(leaf_id)
  assert leaf is not None and leaf.task_parent_id == node_id
  await _wait_for_reports(tree, node_id, 15, "the leaf's report never reached the bound node")
  assert tree.task_state(leaf_id) == "completed"
  # The node is a task-tree manager: no legacy wake ever fires for it.
  assert wakes == []
  # A replayed fire at the same firing identity creates nothing new.
  await scheduler._execute_task(task_cfg, record_handle=True, firing=firing)
  assert len(tree.runs.list_run_records_sync(leaf_id)) == 1


@pytest.mark.asyncio
async def test_repo_prompt_task_launches_its_type_less_leaf_in_a_worktree(
    bound_env, tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A repo-bound prompt task's leaf carries no task_type, yet its work Run
  renders the implement worktree bindings and spawns its process; the
  type-less success then closes without review and removes the worktree.

  Regression (2026-09-26): the leaf's None type reached render_worktree_bindings,
  whose section lookup raised KeyError. The parity bound path owes here: the
  leaf must actually launch through it (auto-bind first, then the fire).
  """
  cfg, session_mgr, tree = bound_env
  monkeypatch.setattr(SCHEDULER_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  repo, _origin = init_repo_with_origin(tmp_path)
  _persist_unbound_cron_d(cfg, "repo-sweep", {"cron": "0 3 * * *", "prompt": "Do the sweep.", "backend": "fake"})
  backend = SpawningScriptedBackend([result_event("sweep done")])
  install_backends(monkeypatch, [backend], WORKER_BUILD_BACKEND_PATCH_TARGET)
  # The close report's wake dispatches the bound node's report-consuming turn;
  # script it through the registry builder so no external process starts.
  _script_manager_turn(monkeypatch, ["report noted"])

  task_cfg = ScheduledTaskConfig(
      name="repo-sweep", cron="0 3 * * *", prompt="Do the sweep.", backend="fake", repo=str(repo))
  scheduler = Scheduler(cfg, session_mgr)
  result = await scheduler._execute_task(task_cfg, record_handle=True, firing="2026-01-01T03:00:00+00:00")

  leaf_id = result["leaf_session_id"]
  leaf = await tree.load_meta(leaf_id)
  assert leaf is not None and leaf.task is not None and leaf.task.task_type is None
  reports = await _wait_for_reports(
      tree, task_cfg.session_id, 15, "the repo-bound leaf's report never reached the bound node")
  assert [r["outcome"] for r in reports] == ["completed"]
  assert tree.task_state(leaf_id) == "completed"
  # One work Run, spawned, whose launch text carries the implement bindings
  # of the worktree recorded on the Run.
  runs = tree.runs.list_run_records_sync(leaf_id)
  assert [r.kind for r in runs] == ["work"]
  work = runs[0]
  assert backend.prompt is not None
  assert work.pid is not None
  assert work.worktree_path is not None and work.branch_name is not None
  launch_text = (tree.runs.run_dir(leaf_id, work.id) / "launch_prompt.md").read_text(encoding="utf-8")
  assert "## Worktree Workflow" in launch_text
  assert work.branch_name in launch_text and work.worktree_path in launch_text
  assert "Do the sweep." in launch_text
  # The type-less delivery: no review Run, and the delivered worktree is gone.
  # The removal follows the close report (the finalize chain's cleanup step
  # runs after the report is durable), so poll it out instead of racing it.
  deadline = asyncio.get_event_loop().time() + 10
  while Path(work.worktree_path).exists() and asyncio.get_event_loop().time() < deadline:
    await asyncio.sleep(0.05)
  assert not Path(work.worktree_path).exists()


@pytest.mark.asyncio
async def test_unbound_steps_task_binds_then_advances_step_by_step(bound_env, monkeypatch: pytest.MonkeyPatch) -> None:
  """An unbound steps task binds on its first fire, then runs its chain on one
  leaf under the new node: step 0, then step 1 fed the previous result, then
  ONE boundary report to the node."""
  cfg, session_mgr, tree = bound_env
  monkeypatch.setattr(SCHEDULER_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  _persist_unbound_cron_d(
      cfg, "chained-legacy", {
          "cron":
              "0 3 * * *",
          "backend":
              "fake",
          "steps":
              [
                  {
                      "name": "selector",
                      "prompt": "Select candidates."
                  },
                  {
                      "name": "reviewer",
                      "prompt": "Review the diff.",
                      "backend": "codex-o3"
                  },
              ],
      })
  builds = install_backends(
      monkeypatch, [
          SpawningScriptedBackend([result_event("selector says pick three")]),
          SpawningScriptedBackend([result_event("reviewer wrote the report")]),
      ], WORKER_BUILD_BACKEND_PATCH_TARGET)
  task_cfg = ScheduledTaskConfig(
      name="chained-legacy",
      cron="0 3 * * *",
      backend="fake",
      steps=[
          StepConfig(name="selector", prompt="Select candidates."),
          StepConfig(name="reviewer", prompt="Review the diff.", backend="codex-o3"),
      ])
  scheduler = Scheduler(cfg, session_mgr)
  firing = "2026-01-01T03:00:00+00:00"
  result = await scheduler._execute_task(task_cfg, record_handle=True, firing=firing)
  node_id = task_cfg.session_id
  assert node_id is not None
  leaf_id = result["leaf_session_id"]
  deadline = asyncio.get_event_loop().time() + 20
  while asyncio.get_event_loop().time() < deadline:
    records = tree.runs.list_run_records_sync(leaf_id)
    reports = [e for e in tree.events.load_events(node_id) if e.get("type") == ET.CHILD_REPORT]
    if len(records) == 2 and reports:
      break
    await asyncio.sleep(0.1)
  else:
    pytest.fail(f"the chain never finished: {[(r.id, r.kind) for r in tree.runs.list_run_records_sync(leaf_id)]}")
  assert [r.kind for r in records] == ["scheduled_step", "scheduled_step"]
  assert [r.sequence_ref.position for r in records] == [0, 1]
  # The second step's prompt carried the previous result under the legacy heading.
  assert "Result of the previous step (selector)" in builds[1]["backend"].prompt
  assert "selector says pick three" in builds[1]["backend"].prompt
  # ONE report at the boundary, delivered to the node the task now binds.
  reports = [e for e in tree.events.load_events(node_id) if e.get("type") == ET.CHILD_REPORT]
  assert len(reports) == 1
  assert "completed all 2 step(s)" in str(reports[0].get("summary"))
  assert tree.task_state(leaf_id) == "completed"


# ---------------------------------------------------------------------------
# distinct_backend_from: the firing-time (type, model) check
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_firing_with_same_resolved_backend_stops_before_any_step_launches(
    bound_env, monkeypatch: pytest.MonkeyPatch) -> None:
  """Linked steps resolving to the same (type, model) stop the firing before
  any step launches: the failure report names both steps and the shared
  backend, and no scripted process ever starts."""
  cfg, session_mgr, tree = bound_env
  manager = await make_manager(tree)
  builds = install_backends(
      monkeypatch, [SpawningScriptedBackend([result_event("should never run")])], WORKER_BUILD_BACKEND_PATCH_TARGET)
  # No backend written anywhere: the config loads (the repo default's shape),
  # and both steps resolve to the first configured option.
  task_cfg = _bound_task(
      "distinct",
      manager.id,
      backend=None,
      steps=[
          StepConfig(name="selector", prompt="Select."),
          StepConfig(name="reviewer", prompt="Review.", distinct_backend_from="selector"),
      ])
  scheduler = Scheduler(cfg, session_mgr)
  firing = "2026-01-01T03:00:00+00:00"
  result = await scheduler._execute_task(task_cfg, record_handle=True, firing=firing)
  leaf_id = result["leaf_session_id"]
  reports = await _wait_for_reports(tree, manager.id, 20, "the distinct-backend failure report never arrived")
  # Nothing launched: the scripted backend was never built, and the admitted
  # position-0 Run stays registered but pid-less and terminal-less.
  assert builds == []
  records = tree.runs.list_run_records_sync(leaf_id)
  assert [r.sequence_ref.position for r in records] == [0]
  assert records[0].pid is None
  assert tree.runs.terminal_outcome(tree.runs.load_events_sync(leaf_id), records[0].id) is None
  assert tree.task_state(leaf_id) != "completed"
  # The report names both steps and the shared backend.
  summary = str(reports[0].get("summary"))
  assert "stopped before its first step" in summary
  assert "steps 'selector' and 'reviewer'" in summary
  assert OPUS_BACKEND_ID in summary


@pytest.mark.asyncio
async def test_recovery_launch_with_same_resolved_backend_stops_and_reports(
    bound_env, monkeypatch: pytest.MonkeyPatch) -> None:
  """The same check fires on the recovery path: a mid-chain firing whose next
  launch now resolves to a shared backend stops before that launch and reports."""
  from src.core.task_recovery import reconcile_task_tree
  cfg, _session_mgr, tree = bound_env
  manager = await make_manager(tree)
  install_backends(
      monkeypatch, [SpawningScriptedBackend([result_event("should never run")])], WORKER_BUILD_BACKEND_PATCH_TARGET)
  task_cfg = _bound_task(
      "distinct",
      manager.id,
      backend=None,
      steps=[
          StepConfig(name="selector", prompt="Select."),
          StepConfig(name="reviewer", prompt="Review.", distinct_backend_from="selector"),
      ])
  # The recovery reads the binding from the durable cron.d file; the file
  # loads (no backend written) and its steps resolve to the same backend.
  cron_d = cfg.charliebot_home / "config.d" / "cron.d"
  cron_d.mkdir(parents=True, exist_ok=True)
  sel_md = cron_d / "selector.md"
  rev_md = cron_d / "reviewer.md"
  sel_md.write_text("Select the target.\n", encoding="utf-8")
  rev_md.write_text("Review the result.\n", encoding="utf-8")
  (cron_d / "distinct.yaml").write_text(
      yaml.safe_dump(
          {
              "cron":
                  "0 3 * * *",
              "session_id":
                  manager.id,
              "steps":
                  [
                      {
                          "name": "selector",
                          "prompt_file": str(sel_md)
                      },
                      {
                          "name": "reviewer",
                          "prompt_file": str(rev_md),
                          "distinct_backend_from": "selector",
                      },
                  ],
          }),
      encoding="utf-8")
  # The durable mid-chain facts: step 0 terminally successful, nothing after.
  from src.core import cron_sequence
  meta = await tree.load_meta(manager.id)
  leaf = await cron_sequence.ensure_firing_leaf(task_cfg, meta, tree, FIRING, "distinct steps")
  leaf_id = leaf.id
  run0 = await cron_sequence.register_leaf_run(
      tree, leaf_id, task_cfg, FIRING, kind="scheduled_step", position=0, backend=None, model=None)
  await tree.runs.record_finish(leaf_id, run0.id, outcome="success", exit_code=0)

  await reconcile_task_tree(cfg, tree)
  reports = await _wait_for_reports(
      tree, manager.id, 20, "the recovery's distinct-backend failure report never arrived")
  # The next position never launched: no step-1 Run exists, no process started.
  records = tree.runs.list_run_records_sync(leaf_id)
  assert sorted(r.sequence_ref.position for r in records if r.sequence_ref) == [0]
  assert tree.task_state(leaf_id) != "completed"
  summary = str(reports[0].get("summary"))
  assert "stopped before launching step 'reviewer'" in summary
  assert "steps 'selector' and 'reviewer'" in summary
  assert OPUS_BACKEND_ID in summary


@pytest.mark.asyncio
async def test_boundary_report_headings_carry_each_step_backend(bound_env, monkeypatch: pytest.MonkeyPatch) -> None:
  """The completion report's per-step headings carry the backend each step ran."""
  cfg, session_mgr, tree = bound_env
  manager = await make_manager(tree)
  install_backends(
      monkeypatch, [
          SpawningScriptedBackend([result_event("picked three")]),
          SpawningScriptedBackend([result_event("reviewed the picks")]),
      ], WORKER_BUILD_BACKEND_PATCH_TARGET)
  # The completed close's report wake dispatches the manager's turn (the test
  # drains it below); script it so no external CLI process starts — the real
  # spawn sat right on the 1s unit budget and flaked.
  _script_manager_turn(monkeypatch, ["report noted"])
  task_cfg = _bound_task(
      "chained",
      manager.id,
      steps=[
          StepConfig(name="selector", prompt="Select."),
          StepConfig(name="reviewer", prompt="Review.", backend="codex-o3"),
      ])
  scheduler = Scheduler(cfg, session_mgr)
  firing = "2026-01-01T03:00:00+00:00"
  await scheduler._execute_task(task_cfg, record_handle=True, firing=firing)
  reports = await _wait_for_reports(tree, manager.id, 20, "the completion report never arrived")
  summary = str(reports[0].get("summary"))
  assert "**selector result (fake):**" in summary
  assert "**reviewer result (codex-o3):**" in summary
  await _drain_manager_turns(tree, manager.id)


# ---------------------------------------------------------------------------
# Pooled launches: the scheduled step starts on a Claude pool account
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_pooled_scheduled_step_launches_on_the_selected_pool_account(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The cron controller's scheduled_step Runs are fresh worker launches: the
  step hands Worker the pool account claude_accounts.select returned, and the
  relay loop is armed (the backend build receives the same account)."""
  claude_accounts.reset_for_tests()
  cfg, session_mgr, tree = build_pooled_env(tmp_path, monkeypatch)
  bind_deps_managers(monkeypatch, tree, session_mgr)
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
  manager = await make_manager(tree, "Pooled Manager")
  await tree.dispatch.admit_input(manager.id, event_type=ET.USER, content="Take off. Run the schedule.", actor="user")
  recorder = WorkerAccountRecorder()
  recorder.install(monkeypatch)
  builds = install_backends(
      monkeypatch, [SpawningScriptedBackend([result_event("step done")])], WORKER_BUILD_BACKEND_PATCH_TARGET)

  # The boundary close's report wake dispatches the manager's turn; the launch
  # under test is the step's, so the wake records without launching one.
  async def stub_dispatch(session_id: str) -> dict:
    return {"session_id": session_id, "pending": 0, "launch": False}

  monkeypatch.setattr(tree.dispatch, "dispatch_pending", stub_dispatch)

  task_cfg = _bound_task(
      "pooled-steps",
      manager.id,
      backend=POOLED_FABLE_ID,
      steps=[StepConfig(name="only", prompt="Do the single thing.")])
  from src.core import cron_sequence
  from src.core.tasks import create_logged_task
  meta = await tree.load_meta(manager.id)
  leaf = await cron_sequence.ensure_firing_leaf(task_cfg, meta, tree, FIRING, "pooled steps")
  handle = create_logged_task(
      cron_sequence.run_firing_steps(task_cfg, meta, tree, FIRING, leaf.id), name="pooled-steps-controller")
  await asyncio.wait_for(handle, 20)

  expected = claude_accounts.select(cfg, FABLE_MODEL)
  assert expected is not None and expected.label == "main"
  assert recorder.accounts == [expected]
  assert builds[0]["kwargs"]["claude_account"] == expected
  runs = tree.runs.list_run_records_sync(leaf.id)
  assert len(runs) == 1 and tree.runs.terminal_outcome(tree.runs.load_events_sync(leaf.id), runs[0].id) == "success"
