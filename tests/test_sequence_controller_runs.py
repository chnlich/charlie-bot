"""Sequence Runs ask their owning controller: the runtime's ask-the-controller
seams (launch context, after-run, recovery) and the no-controller behavior.

The dispatcher never launches a Run that carries a sequence_ref; a finish asks
the owning controller's ``after_run``; recovery asks its ``recover_run`` and
counts one follow-up per True answer. A Run whose sequence kind no registered
controller claims (its package was deleted) is never launched, its finish and
recovery log ``sequence_run_without_controller`` and do nothing else — except
that recovery records ``interrupted`` for a registered-but-unlaunched one, so
the node holds no Run that nothing can finish, and a second pass only logs.
"""

from __future__ import annotations

import json
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import AsyncMock

import pytest
from conftest import create_task
from structlog.testing import capture_logs

from src.infra import event_types as ET
from src.infra.models import RunRecord, SequenceRef, TaskSpec
from src.runtime.runs import RUN_EVENTS_NAME
from tests.test_task_execution import _adapter_with_silent_broadcast, build_env

# A kind no registered controller owns: the owning package was deleted, so its
# registration is gone and the stored Runs it left behind outlive it.
ORPHAN_REF = SequenceRef(kind="deleted_feature", owner_ref="deleted:loop-1", position=1)


async def _manager_and_leaf(tree) -> tuple:
  manager = await create_task(
      tree, parent=None, request_id="root", profile="manager", task=TaskSpec(goal="pm"), name="PM")
  leaf = await create_task(
      tree, parent=manager.id, request_id="leaf", profile="worker", task=TaskSpec(goal="loop work"), name="leaf")
  return manager, leaf


def _without_controller(entries: list[dict]) -> list[dict]:
  return [entry for entry in entries if entry["event"] == "sequence_run_without_controller"]


def _assert_one_warning(entries: list[dict], *, session_id: str, run_id: str, owner_ref: str) -> None:
  """Exactly one no-controller warning, naming the session, run and owner_ref."""
  warnings = _without_controller(entries)
  assert len(warnings) == 1
  assert warnings[0]["session_id"] == session_id
  assert warnings[0]["run_id"] == run_id
  assert warnings[0]["owner_ref"] == owner_ref


@pytest.mark.asyncio
async def test_dispatcher_never_launches_a_queued_sequence_run(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A queued Run with a sequence_ref is its controller's launch: the
  dispatcher skips it instead of starting one headlessly."""
  _cfg, _session_blocks, tree = build_env(tmp_path, monkeypatch)
  _manager, leaf = await _manager_and_leaf(tree)
  run = RunRecord(id="seq-run", session_id=leaf.id, kind="iteration", sequence_ref=ORPHAN_REF)
  await tree.runs.register_run(run)

  await tree.dispatch.dispatch_pending(leaf.id)

  stored = await tree.runs.get_run(leaf.id, "seq-run")
  assert stored is not None
  assert stored.pid is None  # nothing started
  events = tree.runs.load_events_sync(leaf.id)
  assert tree.runs.run_has_terminal_fact(stored, events) is False  # nothing landed


@pytest.mark.asyncio
async def test_a_finished_sequence_run_asks_its_owning_controller_after_run(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A durable finish asks the owning controller's after_run: the cron step
  re-drives its firing, the improve iteration runs its loop's close step —
  which leaves the node open while the loop is still running — and no owned
  Run logs the no-controller warning."""
  import os

  from src.features.cron import sequence_controller
  from src.features.improve.improve_command import ImproveState, save_loop_state
  from src.features.improve.improve_sequence import loop_owner_ref

  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  _manager, leaf = await _manager_and_leaf(tree)
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  adapter = tree.dispatch.executor
  meta = await tree.load_meta(leaf.id)
  cron_step = RunRecord(
      id="step-0",
      session_id=leaf.id,
      kind="scheduled_step",
      backend="fake",
      model="fake-model",
      sequence_ref=SequenceRef(kind="cron_steps", owner_ref="cron:nightly@2026-10-08T00:00Z", position=0))
  iteration = RunRecord(
      id="iter-1",
      session_id=leaf.id,
      kind="iteration",
      backend="fake",
      model="fake-model",
      sequence_ref=SequenceRef(kind="improve", owner_ref=loop_owner_ref(leaf.id, 1, cfg), position=1))
  await tree.runs.register_run(cron_step)
  await tree.runs.register_run(iteration)
  redrive = AsyncMock()
  monkeypatch.setattr(sequence_controller, "redrive_firing", redrive)

  await adapter._after_worker_run(meta, cron_step, "success")
  redrive.assert_awaited_once_with(leaf.id, tree, cfg)

  # A running loop: after_run's close step returns on the running state and
  # the node stays open.
  await save_loop_state(
      leaf.id,
      ImproveState(
          loop_id=1,
          goal="improve the thing",
          status="running",
          work_branch="improve/test",
          repo_path=str(tmp_path),
          created_at="2026-10-08T00:00:00+00:00",
          server_pid=os.getpid()), cfg)
  with capture_logs() as logs:
    await adapter._after_worker_run(meta, iteration, "success")
  assert _without_controller(logs) == []
  assert tree.task_state(leaf.id) == "open"


@pytest.mark.asyncio
async def test_recovery_asks_the_owning_controller_and_counts_one_follow_up(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Recovery replays a terminal cron step and an unlaunched one (one follow-up
  each), and leaves a launched non-terminal step to the ordinary follow."""
  from src.features.cron import sequence_controller
  from src.runtime.task_execution import _replay_followups

  cfg, _session_blocks, tree = build_env(tmp_path, monkeypatch)
  _manager, leaf = await _manager_and_leaf(tree)

  def step(position: str, **fields) -> RunRecord:
    return RunRecord(
        id=f"step-{position}",
        session_id=leaf.id,
        kind="scheduled_step",
        backend="fake",
        model="fake-model",
        sequence_ref=SequenceRef(kind="cron_steps", owner_ref="cron:nightly@2026-10-08T00:00Z", position=0),
        **fields)

  finished = step("fin")
  unlaunched = step("queued")
  launched = step("live", pid=999999, pid_start="1-424000", started_at="2026-10-08T00:00:00+00:00")
  for run in (finished, unlaunched, launched):
    await tree.runs.register_run(run)
  await tree.runs.record_finish(leaf.id, finished.id, "success", exit_code=0)
  redrive = AsyncMock()
  monkeypatch.setattr(sequence_controller, "redrive_firing", redrive)
  counters = {"followups": 0}

  await _replay_followups(leaf.id, tree, None, counters, cfg)

  assert counters["followups"] == 2
  assert redrive.await_count == 2
  # The launched, non-terminal step is followed (or drained) by the earlier
  # pass: recovery neither replays it nor counts it.
  assert tree.runs.run_has_terminal_fact(launched, tree.runs.load_events_sync(leaf.id)) is False


@pytest.mark.asyncio
async def test_a_finish_without_a_controller_warns_and_does_nothing_else(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A Run whose kind no controller owns: its finish logs the warning with the
  session, run and owner_ref, and nothing else happens."""
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  _manager, leaf = await _manager_and_leaf(tree)
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  meta = await tree.load_meta(leaf.id)
  run = RunRecord(
      id="orphan", session_id=leaf.id, kind="iteration", backend="fake", model="fake-model", sequence_ref=ORPHAN_REF)
  await tree.runs.register_run(run)

  with capture_logs() as logs:
    await tree.dispatch.executor._after_worker_run(meta, run, "success")

  _assert_one_warning(logs, session_id=leaf.id, run_id="orphan", owner_ref=ORPHAN_REF.owner_ref)
  # Nothing else: no terminal fact landed from the finish path, and the
  # queued Run stands as it was.
  stored = await tree.runs.get_run(leaf.id, "orphan")
  assert stored is not None
  assert stored.pid is None
  assert tree.runs.run_has_terminal_fact(stored, tree.runs.load_events_sync(leaf.id)) is False


@pytest.mark.asyncio
async def test_recovery_without_a_controller_lands_interrupted_then_only_logs(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Recovery of a registered-but-unlaunched Run without its controller records
  the interrupted finish through the run store's normal entry and logs the same
  warning; a second pass finds the terminal fact and only logs."""
  from src.runtime.task_execution import _replay_followups

  cfg, _session_blocks, tree = build_env(tmp_path, monkeypatch)
  _manager, leaf = await _manager_and_leaf(tree)
  run = RunRecord(
      id="orphan", session_id=leaf.id, kind="iteration", backend="fake", model="fake-model", sequence_ref=ORPHAN_REF)
  await tree.runs.register_run(run)
  counters = {"followups": 0}

  with capture_logs() as logs:
    await _replay_followups(leaf.id, tree, None, counters, cfg)
  assert counters["followups"] == 0
  _assert_one_warning(logs, session_id=leaf.id, run_id="orphan", owner_ref=ORPHAN_REF.owner_ref)
  stored = await tree.runs.get_run(leaf.id, "orphan")
  assert stored is not None and stored.ended_at is not None
  assert await tree.runs.terminal_outcome_of(leaf.id, "orphan") == "interrupted"
  finished_facts = [
      e for e in tree.runs.load_events_sync(leaf.id) if e.get("type") == ET.RUN_FINISHED and e.get("run_id") == "orphan"
  ]
  assert len(finished_facts) == 1

  with capture_logs() as logs:
    await _replay_followups(leaf.id, tree, None, counters, cfg)
  assert counters["followups"] == 0  # a terminal Run only logs
  assert len(_without_controller(logs)) == 1
  finished_facts = [
      e for e in tree.runs.load_events_sync(leaf.id) if e.get("type") == ET.RUN_FINISHED and e.get("run_id") == "orphan"
  ]
  assert len(finished_facts) == 1  # the second pass lands nothing twice


@pytest.mark.asyncio
async def test_recovery_follows_a_live_sequence_run_without_its_controller(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A launched, non-terminal Run without its controller is followed exactly as
  any other live Run; no interrupted finish and no warning."""
  from src.runtime import runs
  from src.runtime.task_execution import _reconcile_node

  cfg, _session_blocks, tree = build_env(tmp_path, monkeypatch)
  _manager, leaf = await _manager_and_leaf(tree)
  run = RunRecord(
      id="orphan",
      session_id=leaf.id,
      kind="iteration",
      backend="fake",
      model="fake-model",
      pid=999999,
      pid_start="1-424000",
      started_at="2026-10-08T00:00:00+00:00",
      sequence_ref=ORPHAN_REF)
  await tree.runs.register_run(run)
  follows: list[tuple[str, str]] = []
  adapter = SimpleNamespace(
      follow_run_in_background=lambda session_id, run_id: follows.append((session_id, run_id)), resume_run=AsyncMock())
  counters = {"resumed": 0, "drained": 0, "followups": 0}

  with capture_logs() as logs:
    monkeypatch.setattr(runs, "is_run_alive", lambda *args: True)
    await _reconcile_node(leaf.id, tree, adapter, counters, cfg)

  assert follows == [(leaf.id, "orphan")]
  assert counters["resumed"] == 1
  assert counters["followups"] == 0
  assert _without_controller(logs) == []
  stored = await tree.runs.get_run(leaf.id, "orphan")
  assert stored is not None
  assert tree.runs.run_has_terminal_fact(stored, tree.runs.load_events_sync(leaf.id)) is False


@pytest.mark.asyncio
async def test_a_launch_without_a_controller_fails_loudly_naming_the_owner_ref(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """If the adapter reaches a controller-less sequence Run anyway, the launch
  raises TaskInvalidError naming the owner_ref and the durable failure lands."""

  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  _manager, leaf = await _manager_and_leaf(tree)
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  run = RunRecord(
      id="orphan", session_id=leaf.id, kind="iteration", backend="fake", model="fake-model", sequence_ref=ORPHAN_REF)
  await tree.runs.register_run(run)

  observation = await tree.dispatch.executor.launch_and_settle(leaf.id, "orphan")

  assert observation.withheld is None
  assert observation.outcome == "failed"
  stored = await tree.runs.get_run(leaf.id, "orphan")
  assert stored is not None and stored.pid is None  # no process ever started
  # The launch's durable evidence: the error rides the run's own events log,
  # where the failure-summary reader already looks.
  events_log = tree.runs.run_dir(leaf.id, "orphan") / RUN_EVENTS_NAME
  error_events = [json.loads(line) for line in events_log.read_text(encoding="utf-8").splitlines() if line.strip()]
  assert [e.get("type") for e in error_events] == [ET.ERROR]
  assert ORPHAN_REF.owner_ref in str(error_events[0].get("message"))
  assert await tree.runs.terminal_outcome_of(leaf.id, "orphan") == "failed"
