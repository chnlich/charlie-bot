"""Bound-scheduled-task tests: one stable node, durable firing identity, Runs.

Exercises the actual scheduler entry points (_maybe_run / run_task_now) against
a synthetic instance with deterministic scripted backend processes: bound
master and worker modes, steps sharing one leaf, per-step backends, stop
semantics, config API round-trip, closed nodes, new-vs-replayed
firings, and run-token spoof attempts.
"""

from __future__ import annotations

import asyncio
import pathlib
import uuid

import conftest
import pytest
import yaml

from src.backends.claude_code import claude_accounts
from src.features.cron import config as cron_config
from src.infra import config, models
from src.infra import event_types as ET
from src.runtime import control_events, task_sessions
from tests import test_task_execution


def build_env(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch):
  """One synthetic home, the fake + codex backends, and one task tree."""
  return test_task_execution.build_spawning_env(
      tmp_path,
      monkeypatch,
      options=[
          conftest.OPUS_BACKEND_OPTION,
          conftest.CODEX_BACKEND_OPTION,
          conftest.backend_option(id="fake", label="Fake", type="codex", model="fake-model"),
      ])


@pytest.fixture()
def bound_env(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch):
  """cfg + tree with the scheduler's deps singleton and the adapter installed.

  The bound manager node is created per test (each test is async).
  """
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  conftest.bind_deps_blocks(monkeypatch, tree, session_blocks)
  tree.dispatch.executor = test_task_execution._adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  return cfg, session_blocks, tree


async def make_manager(tree: task_sessions.TaskTreeManager, name: str = "Manager"):
  return await tree.create_task(
      request_id=f"manager-{name}",
      task_parent_id=None,
      profile="manager",
      task=models.TaskSpec(goal="standup"),
      name=name,
      backend=None,
      caller="operator")


def _bound_task(name: str, session_id: str, **overrides) -> cron_config.ScheduledTaskConfig:
  body = {
      "name": name,
      "cron": "0 3 * * *",
      "session_id": session_id,
      "backend": "fake",
  }
  body.update(overrides)
  return cron_config.ScheduledTaskConfig(**body)


def _persist_chained_cron_d(cfg: config.CharlieBotConfig, session_id: str) -> cron_config.ScheduledTaskConfig:
  """Bind the two-step chained task for `session_id` and persist its durable
  cron.d binding under this instance's home: the two step prompt files and the
  chained.yaml. The recovery reads the binding from that durable file, so it
  must exist on disk before a simulated restart. Returns the in-memory task
  config the scheduler entry points accept."""
  task_cfg = _bound_task(
      "chained",
      session_id,
      steps=[
          cron_config.StepConfig(name="selector", prompt="Select."),
          cron_config.StepConfig(name="reviewer", prompt="Review."),
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


async def _wait_for_child_report(tree, node_id: str, what: str, timeout: float = 20.0) -> list[dict]:
  """Wait out the node's report turn and return its child reports."""
  await conftest._async_wait_for(lambda: bool(conftest.child_reports(tree, node_id)), timeout, what)
  return conftest.child_reports(tree, node_id)


# ---------------------------------------------------------------------------
# mode: master: the typed scheduled input lands on the bound manager
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_bound_master_worker_spoof_cannot_forged_scheduled_input(
    bound_env, monkeypatch: pytest.MonkeyPatch) -> None:
  """The scheduled input's provenance is server-owned: a run-token caller
  cannot mint SCHEDULED_TRIGGER events, and its real messages stay agent ones."""
  _cfg, _session_blocks, tree = bound_env
  manager = await make_manager(tree)
  run_id = control_events.stable_run_id(manager.id, "spoof:work")
  await tree.runs.register_run_locked(models.RunRecord(id=run_id, session_id=manager.id, kind="work"))
  await tree.runs.record_launch(manager.id, run_id, pid=424000, pid_start="ps-1")
  import src.infra.config as core_config
  from src.runtime import run_token
  key = str(core_config.get_credentials().get("charliebot", "access_key") or "")
  claims = run_token.RunTokenClaims(session_id=manager.id, run_id=run_id, agent="Manager")
  token = run_token.sign_run_token(claims, key)
  request = type("R", (), {"headers": {"authorization": f"Bearer {token}"}})()
  from src.runtime.api import deps
  identity = await deps.require_caller(request, tree.runs)
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
  cfg, session_blocks, tree = bound_env
  manager = await make_manager(tree)
  conftest.install_scripted_backends(
      monkeypatch, [
          test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("broke")], exit_code=1),
      ], conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)
  # The failed step's report wakes the manager: its report-consuming turn rides
  # the registry builder (scripted), so no external process starts from this test.
  test_task_execution.script_manager_turn(monkeypatch, ["report noted"])
  task_cfg = _persist_chained_cron_d(cfg, manager.id)
  scheduler = conftest.build_scheduler(cfg, session_blocks)
  firing = "2026-01-01T03:00:00+00:00"
  result = await scheduler._execute_task(task_cfg, record_handle=True, firing=firing)
  leaf_id = result["leaf_session_id"]
  reports = await _wait_for_child_report(tree, manager.id, "the failure report never arrived")
  # The chain stopped at the failed step: no later step run exists.
  records = tree.runs.list_run_records_sync(leaf_id)
  assert [r.sequence_ref.position for r in records] == [0]
  # The failed step's report names the stop, and the report's delivery
  # archives the stopped leaf: exactly one report, then the end state.
  assert "stopped at step 'selector'" in str(reports[0].get("summary"))
  assert len(reports) == 1
  await conftest._async_wait_for(lambda: tree.task_state(leaf_id) == "archived", 15.0, "the failed leaf's archive")
  closes = [e for e in tree.events.load_events(leaf_id) if e["type"] == ET.TASK_CLOSED]
  assert len(closes) == 1
  assert closes[0]["outcome"] == "archived"
  assert closes[0]["actor"] == "system"
  assert closes[0]["request_id"] == f"unsuccessful:{records[0].id}"
  assert closes[0]["run_ids"] == [records[0].id]
  # The manager's report turn settles inside the test: no launch outlives the scripted build.
  await test_task_execution.drain_manager_turns(tree, manager.id)


# ---------------------------------------------------------------------------
# Binding validation: never a replacement session, closed skip
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_missing_binding_fails_visibly_without_creating_a_session(
    bound_env, monkeypatch: pytest.MonkeyPatch) -> None:
  from src.features.cron import cron_sequence
  cfg, session_blocks, tree = bound_env
  await make_manager(tree)
  task_cfg = _bound_task("ghost", str(uuid.uuid4()), prompt="Nobody home.")
  scheduler = conftest.build_scheduler(cfg, session_blocks)
  with pytest.raises(cron_sequence.ScheduledBindingError, match="does not exist"):
    await scheduler._execute_task(task_cfg, record_handle=True, firing="2026-01-01T03:00:00+00:00")
  # No session was created for the missing binding.
  metas = list(cfg.sessions_dir.glob("*/metadata.json"))
  assert len(metas) == 1  # only the manager itself


@pytest.mark.asyncio
async def test_closed_bound_node_generates_no_new_execution(bound_env, monkeypatch: pytest.MonkeyPatch) -> None:
  from src.features.cron import cron_sequence
  cfg, session_blocks, tree = bound_env
  manager = await make_manager(tree)
  conftest.install_scripted_backends(
      monkeypatch, [test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("x")])],
      conftest.BUILD_BACKEND_PATCH_TARGET)
  from src.runtime import task_completion
  close_run = control_events.stable_run_id(manager.id, "close:evidence")
  await tree.runs.register_run_locked(models.RunRecord(id=close_run, session_id=manager.id, kind="manager_turn"))
  await tree.runs.record_launch(manager.id, close_run, pid=424001, pid_start="ps-1")
  await tree.dispatch.finish_run(manager.id, close_run, outcome="success")
  await tree.completion.archive_task(
      manager.id,
      request_id="close-1",
      caller=conftest.OPERATOR,
      evidence=task_completion.CompletionEvidence(
          summary="done", run_ids=[close_run], result_refs=[f"run:{close_run}"]))
  task_cfg = _bound_task("wake-manager", manager.id, prompt="Standup.")
  scheduler = conftest.build_scheduler(cfg, session_blocks)
  with pytest.raises(cron_sequence.ScheduledBindingError, match="no new cron execution"):
    await scheduler._execute_task(task_cfg, record_handle=True, firing="2026-01-01T03:00:00+00:00")
  # The configuration remains readable.
  assert task_cfg.session_id == manager.id


# ---------------------------------------------------------------------------
# Config schema + API round-trip
# ---------------------------------------------------------------------------

# ---------------------------------------------------------------------------
# Withheld launches settle; the checkpoint follows admission
# ---------------------------------------------------------------------------


async def cancel_task_node(tree: task_sessions.TaskTreeManager, session_id: str, request_id: str) -> None:
  """Close one run-less node with outcome cancelled: a closed task withholds
  every launch (a queued Run would block the cancel, so close first)."""
  await tree.completion.cancel_task(
      session_id, request_id=request_id, reason="withhold the launch", caller=conftest.OPERATOR)


async def reopen_task_node(tree: task_sessions.TaskTreeManager, session_id: str, request_id: str) -> None:
  await tree.completion.restore_task(
      session_id, request_id=request_id, reason="precondition cleared", caller=conftest.OPERATOR)


def blocked_reports(tree: task_sessions.TaskTreeManager, manager_id: str) -> list[dict]:
  return [e for e in conftest.child_reports(tree, manager_id) if e.get("outcome") == "blocked"]


@pytest.mark.asyncio
async def test_withheld_step_launch_settles_the_chain_without_hanging(
    bound_env, monkeypatch: pytest.MonkeyPatch) -> None:
  """A step launch whose precondition fails after registration (here: the leaf
  was cancelled after it was created, before its step Run launched) must settle
  the controller explicitly: the step Run stays queued, the actual reason is
  delivered as the stable blocked boundary report, and the scheduler's overlap
  handle ends with the controller instead of hanging."""
  _cfg, _session_blocks, tree = bound_env
  manager = await make_manager(tree)
  builds = conftest.install_scripted_backends(monkeypatch, [], conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)
  task_cfg = _bound_task(
      "withheld-steps",
      manager.id,
      steps=[
          cron_config.StepConfig(name="first", prompt="Do the first thing."),
          cron_config.StepConfig(name="second", prompt="Do the second thing.")
      ])
  from src.features.cron import cron_sequence
  from src.infra import tasks
  meta = await tree.load_meta(manager.id)
  leaf = await cron_sequence.ensure_firing_leaf(task_cfg, meta, tree, FIRING, f"{task_cfg.name} steps")
  await cancel_task_node(tree, leaf.id, "withhold-leaf")
  await cron_sequence.register_leaf_run(
      tree,
      leaf.id,
      task_cfg,
      FIRING,
      kind="scheduled_step",
      position=0,
      backend="fake",
      model="fake-model",
      task_spec_text=task_cfg.steps[0].prompt)

  handle = tasks.create_logged_task(
      cron_sequence.run_firing_steps(task_cfg, meta, tree, FIRING, leaf.id), name="withheld-steps-controller")
  await asyncio.wait_for(handle, 20)
  assert handle.done()

  reports = blocked_reports(tree, manager.id)
  assert len(reports) == 1
  assert "withheld" in str(reports[0]["summary"])
  # The retained pending request: the queued step Run with no terminal fact,
  # settled by the closed node's one system stop request.
  runs = tree.runs.list_run_records_sync(leaf.id)
  assert len(runs) == 1
  assert runs[0].pid is None
  assert tree.runs.terminal_outcome(tree.runs.load_events_sync(leaf.id), runs[0].id) is None
  assert builds == []
  events = tree.runs.load_events_sync(leaf.id)
  stops = [e for e in events if e["type"] == ET.RUN_STOP_REQUESTED and e.get("run_id") == runs[0].id]
  assert len(stops) == 1 and stops[0]["request_id"] == f"closed-task:{runs[0].id}"
  # The stop request holds across the leaf's reopen: the replayed
  # reconciliation re-drives the same queued run and the durable stop refuses
  # the launch — no process, no terminal fact, no duplicate run.
  await reopen_task_node(tree, leaf.id, "clear-leaf")
  assert tree.task_state(leaf.id) == "open"
  builds2 = conftest.install_scripted_backends(
      monkeypatch, [test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("first done")])],
      conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)
  await asyncio.wait_for(cron_sequence.reconcile_bound_firings(task_cfg, meta, tree, FIRING, leaf.id), 20)
  runs_after = tree.runs.list_run_records_sync(leaf.id)
  assert len(runs_after) == 1
  assert runs_after[0].pid is None
  assert tree.runs.terminal_outcome(tree.runs.load_events_sync(leaf.id), runs_after[0].id) is None
  assert builds2 == []


@pytest.mark.asyncio
async def test_steps_admission_failure_does_not_consume_the_occurrence(
    bound_env, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_blocks, tree = bound_env
  manager = await make_manager(tree)
  conftest.install_scripted_backends(monkeypatch, [], conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)
  task_cfg = _bound_task("flaky-steps", manager.id, steps=[cron_config.StepConfig(name="only", prompt="Do it.")])
  scheduler = conftest.build_scheduler(cfg, session_blocks)
  from src.features.cron import scheduler as scheduler_module
  original = scheduler_module.cron_sequence.register_leaf_run

  async def flaky_register(*args, **kwargs):
    raise RuntimeError("run registration exploded")

  monkeypatch.setattr(scheduler_module.cron_sequence, "register_leaf_run", flaky_register)
  with pytest.raises(RuntimeError):
    await scheduler._execute_task(task_cfg, record_handle=True, firing=FIRING)
  after_failure = await tree.load_meta(manager.id)
  assert after_failure.last_scheduled_run is None

  monkeypatch.setattr(scheduler_module.cron_sequence, "register_leaf_run", original)
  await scheduler._execute_task(task_cfg, record_handle=True, firing=FIRING)
  after_success = await tree.load_meta(manager.id)
  assert after_success.last_scheduled_run is not None


FIRING = "2026-01-01T03:00:00+00:00"


# ---------------------------------------------------------------------------
# Recovered boundaries deliver without a new tick; replay stays idempotent
# ---------------------------------------------------------------------------
async def _successful_two_step_leaf(bound_env, monkeypatch: pytest.MonkeyPatch):
  """A leaf whose two steps ran to durable success (no close attempted)."""
  cfg, session_blocks, tree = bound_env
  manager = await make_manager(tree)
  # The leaf's work runs re-judge the nearest-user gate at launch: authorize
  # the manager so the repair run below launches.
  await tree.dispatch.admit_input(manager.id, event_type=ET.USER, content="Take off. Run the schedule.", actor="user")
  conftest.install_scripted_backends(
      monkeypatch, [
          test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("step zero done")]),
          test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("step one done")])
      ], conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)
  # The boundary close dispatches the parent's report-consuming turn through
  # the registry builder: script it so no external process starts here.
  test_task_execution.script_manager_turn(monkeypatch, ["report noted"])
  task_cfg = _bound_task(
      "recovered-boundary",
      manager.id,
      steps=[cron_config.StepConfig(name="zero", prompt="Zero."),
             cron_config.StepConfig(name="one", prompt="One.")])
  from src.features.cron import cron_sequence
  meta = await tree.load_meta(manager.id)
  leaf = await cron_sequence.ensure_firing_leaf(task_cfg, meta, tree, FIRING, "recovered boundary steps")
  leaf_meta = await tree.load_meta(leaf.id)
  for position, (_name, _outcome_text) in enumerate([("zero", "step zero done"), ("one", "step one done")]):
    run = await cron_sequence.register_leaf_run(
        tree,
        leaf.id,
        task_cfg,
        FIRING,
        kind="scheduled_step",
        position=position,
        backend="fake",
        model="fake-model",
        task_spec_text=task_cfg.steps[position].prompt)
    await tree.runs.record_finish(leaf.id, run.id, outcome="success")
  return cfg, session_blocks, tree, manager, task_cfg, meta, leaf, leaf_meta


@pytest.mark.asyncio
async def test_recovered_successful_final_step_close_blocked_delivers_one_blocked_report(
    bound_env, monkeypatch: pytest.MonkeyPatch) -> None:
  """A recovered chain whose final close is blocked leaves the leaf open with
  its evidence intact and delivers the SAME stable blocked report the fresh
  chain delivers; repeated recovery is idempotent; the repaired close later
  follows the normal completion/report policy."""
  _cfg, _session_blocks, tree, manager, task_cfg, meta, leaf, _leaf_meta = (
      await _successful_two_step_leaf(bound_env, monkeypatch))
  # The blocker: an unclaimed pending input on the leaf.
  await tree.dispatch.admit_input(
      leaf.id,
      event_type=ET.AGENT_MESSAGE,
      content="one more thing",
      actor="agent",
      from_session=manager.id,
      from_session_name="Manager")

  from src.features.cron import cron_sequence
  await cron_sequence.reconcile_bound_firings(task_cfg, meta, tree, FIRING, leaf.id)
  reports = conftest.child_reports(tree, manager.id)
  assert len(reports) == 1
  assert reports[0]["outcome"] == "blocked"
  assert "blocked" in str(reports[0]["summary"])
  assert tree.task_state(leaf.id) == "open"

  # Repeated recovery at the blocked-close window adds nothing.
  await cron_sequence.reconcile_bound_firings(task_cfg, meta, tree, FIRING, leaf.id)
  await cron_sequence.reconcile_bound_firings(task_cfg, meta, tree, FIRING, leaf.id)
  assert len(conftest.child_reports(tree, manager.id)) == 1
  assert len([e for e in tree.events.load_events(leaf.id) if e.get("type") == ET.TASK_CLOSED]) == 0

  # The repaired close: consume the pending input with a successful run; the
  # normal completion owner closes and delivers the completed report.
  builds = conftest.install_scripted_backends(
      monkeypatch, [test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("answered")])],
      conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)
  decision = await tree.dispatch.dispatch_pending(leaf.id)
  assert decision["launch"] is True
  await conftest._async_wait_for(
      lambda: tree.task_state(leaf.id) != "open" and len(conftest.child_reports(tree, manager.id)) == 2,
      15.0,
      "the repaired close and its parent report never landed",
  )
  assert len(builds) == 1
  kinds = [(e.get("outcome"), str(e.get("summary"))[:60]) for e in conftest.child_reports(tree, manager.id)]
  assert len(kinds) == 2
  assert sorted(o for o, _ in kinds) == ["blocked", "completed"]
  assert tree.task_state(leaf.id) == "completed"
  await test_task_execution.drain_manager_turns(tree, manager.id)


@pytest.mark.asyncio
async def test_recovered_final_step_boundary_settles_without_a_new_tick(
    bound_env, monkeypatch: pytest.MonkeyPatch) -> None:
  """Recovery redrives a finished chain's boundary directly: the close and the
  ONE report land in the same pass, with no scheduler tick or restart."""
  _cfg, _session_blocks, tree, manager, task_cfg, meta, leaf, _leaf_meta = (
      await _successful_two_step_leaf(bound_env, monkeypatch))
  from src.features.cron import cron_sequence
  await cron_sequence.reconcile_bound_firings(task_cfg, meta, tree, FIRING, leaf.id)
  assert tree.task_state(leaf.id) == "completed"
  reports = conftest.child_reports(tree, manager.id)
  assert len(reports) == 1
  assert "completed all 2 step(s)" in str(reports[0].get("summary"))
  await test_task_execution.drain_manager_turns(tree, manager.id)


@pytest.mark.asyncio
async def test_simultaneous_fresh_and_recovery_followup_produce_no_duplicate(
    bound_env, monkeypatch: pytest.MonkeyPatch) -> None:
  """Two concurrent follow-ups at the frontier (a repeated recovery scan) land
  one next step, one process, one close, one report."""
  _cfg, _session_blocks, tree, manager, task_cfg, meta, leaf, _leaf_meta = (
      await _successful_two_step_leaf(bound_env, monkeypatch))
  from src.features.cron import cron_sequence
  await asyncio.gather(
      cron_sequence.reconcile_bound_firings(task_cfg, meta, tree, FIRING, leaf.id),
      cron_sequence.reconcile_bound_firings(task_cfg, meta, tree, FIRING, leaf.id),
  )
  # Both scans converge on the same boundary product (stable close/report ids).
  assert len(conftest.child_reports(tree, manager.id)) == 1
  assert len([e for e in tree.events.load_events(leaf.id) if e.get("type") == ET.TASK_CLOSED]) == 1
  assert len(tree.runs.list_run_records_sync(leaf.id)) == 2
  await test_task_execution.drain_manager_turns(tree, manager.id)


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
  _cfg, _session_blocks, tree, manager, task_cfg, meta, leaf, _leaf_meta = (
      await _successful_two_step_leaf(bound_env, monkeypatch))
  # The parent wake fails exactly once, after the close and its report landed:
  # the executor raises before reserving, so the pending batch stays pending.
  from src.runtime import task_execution
  orig_call = task_execution.TaskExecutionAdapter.__call__
  calls = {"n": 0}

  async def failing_call(self, session_id, pending, *, launch_run_id=None):
    assert session_id == manager.id
    calls["n"] += 1
    raise RuntimeError("dispatch reserved run x against a batch that vanished within one lock hold")

  monkeypatch.setattr(task_execution.TaskExecutionAdapter, "__call__", failing_call)
  from src.features.cron import cron_sequence
  await cron_sequence.reconcile_bound_firings(task_cfg, meta, tree, FIRING, leaf.id)
  assert calls["n"] == 1
  # The close landed and the boundary product is exactly ONE completed report.
  assert tree.task_state(leaf.id) == "completed"
  reports = conftest.child_reports(tree, manager.id)
  assert len(reports) == 1
  assert reports[0]["outcome"] == "completed"
  assert len([e for e in tree.events.load_events(leaf.id) if e.get("type") == ET.TASK_CLOSED]) == 1

  # The wake is re-drivable: the delivered report is the parent's durable
  # input, and the next dispatch consumes it through the scripted turn.
  monkeypatch.setattr(task_execution.TaskExecutionAdapter, "__call__", orig_call)
  decision = await tree.dispatch.dispatch_pending(manager.id)
  assert decision["launch"] is True
  await test_task_execution.drain_manager_turns(tree, manager.id)
  assert len(conftest.child_reports(tree, manager.id)) == 1
  assert len(tree.runs.list_run_records_sync(leaf.id)) == 2


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
  cfg, session_blocks, tree = bound_env
  # The unbound path binds through the scheduler's reloaded process config;
  # pin it to the synthetic home's cfg.
  monkeypatch.setattr(conftest.SCHEDULER_LOAD_CONFIG_PATCH_TARGET, lambda: cfg)
  conftest.install_scripted_backends(
      monkeypatch, [
          test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("sweep done")]),
      ], conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)
  _persist_unbound_cron_d(cfg, "nightly-sweep", {"cron": "0 3 * * *", "prompt": "Do the sweep.", "backend": "fake"})
  task_cfg = cron_config.ScheduledTaskConfig(
      name="nightly-sweep", cron="0 3 * * *", prompt="Do the sweep.", backend="fake")
  scheduler = conftest.build_scheduler(cfg, session_blocks)
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
  await _wait_for_child_report(tree, node_id, "the leaf's report never reached the bound node", timeout=15.0)
  assert tree.task_state(leaf_id) == "completed"
  # The work Run pins its configured prompt verbatim as task_spec.md: the link
  # the header's "Task spec (not recorded)" placeholder resolves once a run
  # carries it.
  work_run = tree.runs.list_run_records_sync(leaf_id)[0]
  assert work_run.task_spec_ref is not None
  spec_path = pathlib.Path(work_run.task_spec_ref)
  assert spec_path.name == "task_spec.md" and spec_path.is_file()
  assert spec_path.read_text(encoding="utf-8") == "Do the sweep."
  assert work_run.task_spec_hash == control_events.sha256_hex("Do the sweep.")
  # The node receives the leaf report through task-tree dispatch.
  # A replayed fire at the same firing identity creates nothing new.
  await scheduler._execute_task(task_cfg, record_handle=True, firing=firing)
  assert len(tree.runs.list_run_records_sync(leaf_id)) == 1


@pytest.mark.asyncio
async def test_repo_prompt_task_launches_its_type_less_leaf_in_a_worktree(
    bound_env, tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A repo-bound prompt task's leaf carries no task_type, yet its work Run
  renders the implement worktree bindings and spawns its process; the
  type-less success then closes without review and removes the worktree.

  Regression (2026-09-26): the leaf's None type reached render_worktree_bindings,
  whose section lookup raised KeyError. The parity bound path owes here: the
  leaf must actually launch through it (auto-bind first, then the fire).
  """
  cfg, session_blocks, tree = bound_env
  monkeypatch.setattr(conftest.SCHEDULER_LOAD_CONFIG_PATCH_TARGET, lambda: cfg)
  repo, _origin = conftest.init_repo_with_origin(tmp_path)
  _persist_unbound_cron_d(cfg, "repo-sweep", {"cron": "0 3 * * *", "prompt": "Do the sweep.", "backend": "fake"})
  backend = test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("sweep done")])
  conftest.install_scripted_backends(monkeypatch, [backend], conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)
  # The close report's wake dispatches the bound node's report-consuming turn;
  # script it through the registry builder so no external process starts.
  test_task_execution.script_manager_turn(monkeypatch, ["report noted"])

  task_cfg = cron_config.ScheduledTaskConfig(
      name="repo-sweep", cron="0 3 * * *", prompt="Do the sweep.", backend="fake", repo=str(repo))
  scheduler = conftest.build_scheduler(cfg, session_blocks)
  result = await scheduler._execute_task(task_cfg, record_handle=True, firing="2026-01-01T03:00:00+00:00")

  leaf_id = result["leaf_session_id"]
  leaf = await tree.load_meta(leaf_id)
  assert leaf is not None and leaf.task is not None and leaf.task.task_type is None
  reports = await _wait_for_child_report(
      tree, task_cfg.session_id, "the repo-bound leaf's report never reached the bound node", timeout=15.0)
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
  await conftest._async_wait_for(
      lambda: not pathlib.Path(work.worktree_path).exists(), 10.0, "the delivered worktree was never removed")


@pytest.mark.asyncio
async def test_unbound_steps_task_binds_then_advances_step_by_step(bound_env, monkeypatch: pytest.MonkeyPatch) -> None:
  """An unbound steps task binds on its first fire, then runs its chain on one
  leaf under the new node: step 0, then step 1 fed the previous result, then
  ONE boundary report to the node."""
  cfg, session_blocks, tree = bound_env
  monkeypatch.setattr(conftest.SCHEDULER_LOAD_CONFIG_PATCH_TARGET, lambda: cfg)
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
  builds = conftest.install_scripted_backends(
      monkeypatch, [
          test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("selector says pick three")]),
          test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("reviewer wrote the report")]),
      ], conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)
  task_cfg = cron_config.ScheduledTaskConfig(
      name="chained-legacy",
      cron="0 3 * * *",
      backend="fake",
      steps=[
          cron_config.StepConfig(name="selector", prompt="Select candidates."),
          cron_config.StepConfig(name="reviewer", prompt="Review the diff.", backend="codex-o3"),
      ])
  scheduler = conftest.build_scheduler(cfg, session_blocks)
  firing = "2026-01-01T03:00:00+00:00"
  result = await scheduler._execute_task(task_cfg, record_handle=True, firing=firing)
  node_id = task_cfg.session_id
  assert node_id is not None
  leaf_id = result["leaf_session_id"]
  deadline = asyncio.get_event_loop().time() + 20
  while asyncio.get_event_loop().time() < deadline:
    records = tree.runs.list_run_records_sync(leaf_id)
    reports = conftest.child_reports(tree, node_id)
    if len(records) == 2 and reports:
      break
    await asyncio.sleep(0.1)
  else:
    pytest.fail(f"the chain never finished: {[(r.id, r.kind) for r in tree.runs.list_run_records_sync(leaf_id)]}")
  assert [r.kind for r in records] == ["scheduled_step", "scheduled_step"]
  assert [r.sequence_ref.position for r in records] == [0, 1]
  # Each step's Run pins its own configured prompt verbatim (before chaining).
  for run, prompt in zip(records, ["Select candidates.", "Review the diff."], strict=True):
    spec_path = pathlib.Path(run.task_spec_ref or "")
    assert spec_path.name == "task_spec.md" and spec_path.is_file()
    assert spec_path.read_text(encoding="utf-8") == prompt
  # The second step's prompt carried the previous result under the legacy heading.
  assert "Result of the previous step (selector)" in builds[1]["backend"].prompt
  assert "selector says pick three" in builds[1]["backend"].prompt
  # ONE report at the boundary, delivered to the node the task now binds.
  reports = conftest.child_reports(tree, node_id)
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
  backend, and no step process ever starts."""
  cfg, session_blocks, tree = bound_env
  manager = await make_manager(tree)
  builds = conftest.install_scripted_backends(
      monkeypatch,
      [test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("should never run")])],
      conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)
  # The failure report wakes the manager: its report-consuming turn rides the
  # registry builder (scripted), so no external process starts from this test.
  test_task_execution.script_manager_turn(monkeypatch, ["report noted"])
  # No backend written anywhere: the config loads (the repo default's shape),
  # and both steps resolve to the first configured option.
  task_cfg = _bound_task(
      "distinct",
      manager.id,
      backend=None,
      steps=[
          cron_config.StepConfig(name="selector", prompt="Select."),
          cron_config.StepConfig(name="reviewer", prompt="Review.", distinct_backend_from="selector"),
      ])
  scheduler = conftest.build_scheduler(cfg, session_blocks)
  firing = "2026-01-01T03:00:00+00:00"
  result = await scheduler._execute_task(task_cfg, record_handle=True, firing=firing)
  leaf_id = result["leaf_session_id"]
  reports = await _wait_for_child_report(tree, manager.id, "the distinct-backend failure report never arrived")
  # Nothing launched: the scripted backend was never built, and the admitted
  # position-0 Run stays registered but pid-less and terminal-less.
  assert builds == []
  records = tree.runs.list_run_records_sync(leaf_id)
  assert [r.sequence_ref.position for r in records] == [0]
  assert records[0].pid is None
  assert tree.runs.terminal_outcome(tree.runs.load_events_sync(leaf_id), records[0].id) is None
  # The retained queued step Run keeps the leaf open: the archive entry
  # refuses a node with a queued run.
  assert tree.task_state(leaf_id) == "open"
  # The report names both steps and the shared backend.
  summary = str(reports[0].get("summary"))
  assert "stopped before its first step" in summary
  assert "steps 'selector' and 'reviewer'" in summary
  assert conftest.OPUS_BACKEND_ID in summary
  await test_task_execution.drain_manager_turns(tree, manager.id)


@pytest.mark.asyncio
async def test_boundary_report_headings_carry_each_step_backend(bound_env, monkeypatch: pytest.MonkeyPatch) -> None:
  """The completion report's per-step headings carry the backend each step ran."""
  cfg, session_blocks, tree = bound_env
  manager = await make_manager(tree)
  conftest.install_scripted_backends(
      monkeypatch, [
          test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("picked three")]),
          test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("reviewed the picks")]),
      ], conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)
  # The completed close's report wake dispatches the manager's turn (the test
  # drains it below); script it so no external CLI process starts — a real
  # spawn's process start spends the unit budget's headroom and flakes under
  # CI jitter.
  test_task_execution.script_manager_turn(monkeypatch, ["report noted"])
  task_cfg = _bound_task(
      "chained",
      manager.id,
      steps=[
          cron_config.StepConfig(name="selector", prompt="Select."),
          cron_config.StepConfig(name="reviewer", prompt="Review.", backend="codex-o3"),
      ])
  scheduler = conftest.build_scheduler(cfg, session_blocks)
  firing = "2026-01-01T03:00:00+00:00"
  await scheduler._execute_task(task_cfg, record_handle=True, firing=firing)
  reports = await _wait_for_child_report(tree, manager.id, "the completion report never arrived")
  summary = str(reports[0].get("summary"))
  assert "**selector result (fake):**" in summary
  assert "**reviewer result (codex-o3):**" in summary
  await test_task_execution.drain_manager_turns(tree, manager.id)


# ---------------------------------------------------------------------------
# Pooled launches: the scheduled step starts on a Claude pool account
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_pooled_scheduled_step_launches_on_the_selected_pool_account(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The cron controller's scheduled_step Runs are fresh worker launches: the
  lifecycle selects the pool account, and the relay loop is armed (the backend
  build receives the selected account)."""
  conftest.reset_claude_account_readings()
  cfg, session_blocks, tree = test_task_execution.build_pooled_env(tmp_path, monkeypatch)
  conftest.bind_deps_blocks(monkeypatch, tree, session_blocks)
  tree.dispatch.executor = test_task_execution._adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  manager = await make_manager(tree, "Pooled Manager")
  await tree.dispatch.admit_input(manager.id, event_type=ET.USER, content="Take off. Run the schedule.", actor="user")
  builds = conftest.install_scripted_backends(
      monkeypatch, [test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("step done")])],
      conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)

  # The boundary close's report wake dispatches the manager's turn; the launch
  # under test is the step's, so the wake records without launching one.
  async def stub_dispatch(session_id: str) -> dict:
    return {"session_id": session_id, "pending": 0, "launch": False}

  monkeypatch.setattr(tree.dispatch, "dispatch_pending", stub_dispatch)

  task_cfg = _bound_task(
      "pooled-steps",
      manager.id,
      backend=conftest.POOLED_FABLE_ID,
      steps=[cron_config.StepConfig(name="only", prompt="Do the single thing.")])
  from src.features.cron import cron_sequence
  from src.infra import tasks
  meta = await tree.load_meta(manager.id)
  leaf = await cron_sequence.ensure_firing_leaf(task_cfg, meta, tree, FIRING, "pooled steps")
  handle = tasks.create_logged_task(
      cron_sequence.run_firing_steps(task_cfg, meta, tree, FIRING, leaf.id), name="pooled-steps-controller")
  await asyncio.wait_for(handle, 20)

  expected = claude_accounts.select(cfg, conftest.FABLE_MODEL)
  assert expected is not None and expected.label == "main"
  assert builds[0]["kwargs"]["claude_account"] == expected
  runs = tree.runs.list_run_records_sync(leaf.id)
  assert len(runs) == 1 and tree.runs.terminal_outcome(tree.runs.load_events_sync(leaf.id), runs[0].id) == "success"
