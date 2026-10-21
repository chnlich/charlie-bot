"""Boot-recovery marker and closed-node skip in reconcile_task_tree.

Pass 1 writes the marker only when no node failed; pass 2 skips a closed node
that meets all six conditions and checks in full a node that fails any one of
them alone. The end-landing retry path keeps checking a node the boot pass
would skip.
"""

from __future__ import annotations

import asyncio
import datetime
import json
import os
import pathlib

import conftest
import pytest

import src.runtime.task_execution as task_execution_module
from src.infra import event_types as ET
from src.infra import models
from src.runtime import control_events, runs, task_recovery
from tests.test_task_recovery import _takeoff_manager


def _marker_path(cfg) -> pathlib.Path:
  return cfg.charliebot_home / "state" / "boot_recovery.json"


def _read_marker(cfg) -> dict:
  return json.loads(_marker_path(cfg).read_text(encoding="utf-8"))


async def _closed_worker(tmp_path, monkeypatch, *, outcome="success", cold_read=False):
  """Manager + one closed worker child whose only work Run already ended."""
  cfg, _blocks, tree, manager = await _takeoff_manager(tmp_path, monkeypatch)
  if cold_read:
    page = tmp_path / "page.html"
    page.write_text("<html><body><p>body</p></body></html>", encoding="utf-8")
    spec = models.TaskSpec(
        goal="cold read",
        task_type=models.TaskType.COLD_READ,
        cold_read=models.ColdReadSpec(page_path=str(page), genre="sitrep", trigger="where are we?"))
  else:
    spec = models.TaskSpec(goal="do the work")
  worker = await tree.create_task(
      request_id="child",
      task_parent_id=manager.id,
      profile="worker",
      task=spec,
      name="W",
      backend=None,
      caller=conftest.OPERATOR)
  run = await tree.runs.register_run(
      models.RunRecord(id="run-1", session_id=worker.id, kind="work", backend="fake", model="fake-model"))
  await tree.runs.record_finish(worker.id, run.id, outcome, ended_at=models.utc_now())
  await tree.events.append(
      worker.id,
      control_events.build_control_event(
          ET.TASK_CLOSED,
          actor="operator",
          source_session_id=worker.id,
          request_id="close-1",
          outcome="completed" if outcome == "success" else "failed"))
  return cfg, tree, manager, worker


class _NodeSpy:
  """Counts the boot pass's full checks per session id."""

  def __init__(self):
    self.checked: list[str] = []
    self._orig = task_execution_module._reconcile_node

  async def __call__(self, session_id, *args, **kwargs):
    self.checked.append(session_id)
    return await self._orig(session_id, *args, **kwargs)

  def __enter__(self):
    task_execution_module._reconcile_node = self
    return self

  def __exit__(self, *exc):
    task_execution_module._reconcile_node = self._orig


async def _pass_checked(cfg, tree) -> list[str]:
  spy = _NodeSpy()
  with spy:
    await task_recovery.reconcile_task_tree(cfg, tree)
  return spy.checked


@pytest.mark.asyncio
async def test_clean_pass_writes_marker_and_second_pass_skips_closed_node(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, tree, manager, worker = await _closed_worker(tmp_path, monkeypatch)

  first = await _pass_checked(cfg, tree)
  # Pass 1 has no marker yet: every node is checked in full.
  assert sorted(first) == sorted([manager.id, worker.id])
  marker = _read_marker(cfg)
  assert marker["pass_started_at"]
  before = marker["pass_started_at"]

  second = await _pass_checked(cfg, tree)
  # Pass 2 skips the closed worker; the open manager is still checked.
  assert second == [manager.id]
  assert _read_marker(cfg)["pass_started_at"] > before


@pytest.mark.asyncio
async def test_failed_node_leaves_marker_unchanged(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, tree, _manager, _worker = await _closed_worker(tmp_path, monkeypatch)
  await _pass_checked(cfg, tree)
  before = _read_marker(cfg)["pass_started_at"]

  real = task_execution_module._reconcile_node

  async def failing(session_id, *args, **kwargs):
    raise RuntimeError("boom")

  monkeypatch.setattr(task_execution_module, "_reconcile_node", failing)
  try:
    # The pass logs a failing node and continues; it must not raise.
    await task_recovery.reconcile_task_tree(cfg, tree)
  finally:
    monkeypatch.undo()
  assert task_execution_module._reconcile_node is real
  assert _read_marker(cfg)["pass_started_at"] == before


@pytest.mark.asyncio
@pytest.mark.parametrize(
    "condition", [
        "state_open",
        "file_after_marker",
        "run_unended",
        "sequence_ref",
        "worktree_exists",
        "cold_read_failed",
    ])
async def test_each_skip_condition_broken_alone_forces_full_check(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, condition: str) -> None:
  cfg, tree, _manager, worker = await _closed_worker(
      tmp_path,
      monkeypatch,
      outcome="failed" if condition == "cold_read_failed" else "success",
      cold_read=condition == "cold_read_failed")

  if condition == "state_open":
    await tree.events.append(
        worker.id,
        control_events.build_control_event(
            ET.TASK_REOPENED, actor="operator", source_session_id=worker.id, request_id="reopen-1"))
  elif condition == "run_unended":
    await tree.runs.register_run(
        models.RunRecord(id="run-queued", session_id=worker.id, kind="work", backend="fake", model="fake-model"))
  elif condition == "sequence_ref":
    await tree.runs.register_run(
        models.RunRecord(
            id="run-seq",
            session_id=worker.id,
            kind="work",
            backend="fake",
            model="fake-model",
            sequence_ref=models.SequenceRef(kind="retry", owner_ref="owner-1", position=0)))
  elif condition == "worktree_exists":
    worktree = tmp_path / "wt-live"
    worktree.mkdir()
    await tree.runs.register_run(
        models.RunRecord(
            id="run-wt",
            session_id=worker.id,
            kind="work",
            backend="fake",
            model="fake-model",
            worktree_path=str(worktree)))

  await _pass_checked(cfg, tree)
  if condition == "file_after_marker":
    stamp = datetime.datetime.now(datetime.timezone.utc).timestamp() + 60
    node = cfg.sessions_dir / worker.id
    os.utime(node / runs.METADATA_NAME, (stamp, stamp))

  checked = await _pass_checked(cfg, tree)
  assert worker.id in checked


@pytest.mark.asyncio
async def test_end_landing_retry_checks_a_node_the_boot_pass_skips(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, tree, _manager, worker = await _closed_worker(tmp_path, monkeypatch)
  await _pass_checked(cfg, tree)

  spy = _NodeSpy()
  executor = tree.dispatch.executor
  with spy:
    # The retry path drives the node in full, with no skip.
    await asyncio.wait_for(executor._run_end_landing_retry(worker.id, None), timeout=10)
  assert spy.checked == [worker.id]


@pytest.mark.asyncio
async def test_node_check_scan_count_does_not_grow_with_run_count(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """One event list folds once no matter how many Runs ask about it."""
  import test_task_recovery

  import src.runtime.runs as runs_mod

  async def _pass_stats(run_count: int) -> tuple[int, int]:
    cfg, _session_blocks, tree, _manager, worker = await test_task_recovery._manager_and_worker(tmp_path, monkeypatch)
    for i in range(run_count):
      run_id = f"run-{i}"
      await tree.runs.register_run(
          models.RunRecord(id=run_id, session_id=worker.id, kind="work", backend="fake", model="fake-model"))
      await tree.runs.record_finish(worker.id, run_id, "success")

    folds: dict[int, int] = {}
    orig_fold = runs_mod.fold_run_facts
    orig_terminal = runs_mod.terminal_outcome_in_events
    orig_stop = runs_mod.stop_requested_in_events

    def counting_fold(facts, events):
      folds[id(events)] = folds.get(id(events), 0) + 1
      return orig_fold(facts, events)

    questions = {"n": 0}

    def counting_terminal(events, run_id):
      questions["n"] += 1
      return orig_terminal(events, run_id)

    def counting_stop(events, run_id, request_id=None):
      questions["n"] += 1
      return orig_stop(events, run_id, request_id)

    monkeypatch.setattr(runs_mod, "fold_run_facts", counting_fold)
    monkeypatch.setattr(runs_mod, "terminal_outcome_in_events", counting_terminal)
    monkeypatch.setattr(runs_mod, "stop_requested_in_events", counting_stop)
    await task_recovery.reconcile_task_tree(cfg, tree)
    monkeypatch.undo()
    assert questions["n"] >= 2 * run_count
    assert max(folds.values()) == 1
    return max(folds.values()), questions["n"]

  max_folds_one, _ = await _pass_stats(1)
  max_folds_four, _ = await _pass_stats(4)
  assert max_folds_four == max_folds_one == 1
