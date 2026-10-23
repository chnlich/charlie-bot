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
from src.infra import config, models
from src.infra import event_types as ET
from src.runtime import control_events, runs, task_recovery
from tests import test_task_recovery


def _marker_path(cfg) -> pathlib.Path:
  return cfg.charliebot_home / "state" / "boot_recovery.json"


def _read_marker(cfg) -> dict:
  return json.loads(_marker_path(cfg).read_text(encoding="utf-8"))


async def _closed_worker(tmp_path, monkeypatch, *, outcome="success", cold_read=False):
  """Manager + one closed worker child whose only work Run already ended."""
  cfg, _blocks, tree, manager = await test_task_recovery._takeoff_manager(tmp_path, monkeypatch)
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
  run = await tree.runs.register_run_locked(
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


class _ReasonSpy:
  """Records the keep reason the pass decided for each session id (None skips)."""

  def __init__(self):
    self.reasons: dict[str, str | None] = {}
    self._orig = task_recovery._boot_skip_reason

  def __call__(self, tree, session_id, session_dir, meta, marker):
    reason = self._orig(tree, session_id, session_dir, meta, marker)
    self.reasons[session_id] = reason
    return reason

  def __enter__(self):
    task_recovery._boot_skip_reason = self
    return self

  def __exit__(self, *exc):
    task_recovery._boot_skip_reason = self._orig


async def _pass_reasons(cfg, tree) -> tuple[list[str], dict[str, str | None]]:
  checked, reasons = _NodeSpy(), _ReasonSpy()
  with checked, reasons:
    await task_recovery.reconcile_task_tree(cfg, tree)
  return checked.checked, reasons.reasons


def _marker_ts(cfg) -> float:
  return datetime.datetime.fromisoformat(_read_marker(cfg)["pass_started_at"]).timestamp()


def _stamp_durable_files(cfg, session_id: str, offset_s: float) -> None:
  """Set both durable files' mtimes ``offset_s`` from the recorded marker.

  A skip needs the files more than the slack before the marker: a fast test
  writes its files within the slack of the next pass start.
  """
  stamp = _marker_ts(cfg) + offset_s
  node = cfg.sessions_dir / session_id
  for path in (node / runs.DATA_DIR_NAME / "chat_events.jsonl", node / runs.METADATA_NAME):
    os.utime(path, (stamp, stamp))


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

  _stamp_durable_files(cfg, worker.id, offset_s=-2.0)
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
  """Pass 2 checks the node and records the one condition broken on its own."""
  cfg, tree, _manager, worker = await _closed_worker(
      tmp_path,
      monkeypatch,
      outcome="failed" if condition == "cold_read_failed" else "success",
      cold_read=condition == "cold_read_failed")

  # Pass 1 runs with all six conditions holding, so it records a clean marker.
  await _pass_checked(cfg, tree)

  if condition == "state_open":
    # Opening the node before pass 1 would let pass 1 close it again, leaving
    # pass 2 to test the file condition instead.
    await tree.events.append(
        worker.id,
        control_events.build_control_event(
            ET.TASK_REOPENED, actor="operator", source_session_id=worker.id, request_id="reopen-1"))
  elif condition == "run_unended":
    await tree.runs.register_run_locked(
        models.RunRecord(id="run-queued", session_id=worker.id, kind="work", backend="fake", model="fake-model"))
  elif condition == "sequence_ref":
    await tree.runs.register_run_locked(
        models.RunRecord(
            id="run-seq",
            session_id=worker.id,
            kind="work",
            backend="fake",
            model="fake-model",
            sequence_ref=models.SequenceRef(kind="retry", owner_ref="owner-1", position=0)))
    await tree.runs.record_finish(worker.id, "run-seq", "success", ended_at=models.utc_now())
  elif condition == "worktree_exists":
    worktree = tmp_path / "wt-live"
    worktree.mkdir()
    await tree.runs.register_run_locked(
        models.RunRecord(
            id="run-wt",
            session_id=worker.id,
            kind="work",
            backend="fake",
            model="fake-model",
            worktree_path=str(worktree)))
    await tree.runs.record_finish(worker.id, "run-wt", "success", ended_at=models.utc_now())

  # A file newer than the marker keeps the node; every other case backdates
  # its files past the slack so only its own condition is left broken.
  _stamp_durable_files(cfg, worker.id, offset_s=60.0 if condition == "file_after_marker" else -2.0)

  checked, reasons = await _pass_reasons(cfg, tree)
  assert worker.id in checked
  assert reasons[worker.id] == condition


@pytest.mark.asyncio
@pytest.mark.parametrize("offset_s, reason", [(-0.5, "file_after_marker"), (-1.5, None)])
async def test_mtime_slack_bounds_the_marker_comparison(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, offset_s: float, reason: str | None) -> None:
  """A file 0.5 s before the marker keeps the node; 1.5 s before skips it."""
  cfg, tree, _manager, worker = await _closed_worker(tmp_path, monkeypatch)
  await _pass_checked(cfg, tree)
  _stamp_durable_files(cfg, worker.id, offset_s=offset_s)

  checked, reasons = await _pass_reasons(cfg, tree)
  assert reasons[worker.id] == reason
  assert (worker.id in checked) == (reason is not None)


@pytest.mark.asyncio
async def test_end_landing_retry_checks_a_node_the_boot_pass_skips(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, tree, _manager, worker = await _closed_worker(tmp_path, monkeypatch)
  await _pass_checked(cfg, tree)
  _stamp_durable_files(cfg, worker.id, offset_s=-2.0)

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
  import src.runtime.runs as runs_mod

  async def _pass_stats(run_count: int) -> tuple[int, int]:
    # Each measurement gets its own home: a shared one hands the second
    # take-off the first environment's unacknowledged input and its Run, and
    # the take-off then finds the node already consumed.
    home = tmp_path / f"runs-{run_count}"
    home.mkdir()
    cfg, _session_blocks, tree, _manager, worker = await test_task_recovery._manager_and_worker(home, monkeypatch)
    for i in range(run_count):
      run_id = f"run-{i}"
      await tree.runs.register_run_locked(
          models.RunRecord(id=run_id, session_id=worker.id, kind="work", backend="fake", model="fake-model"))
      await tree.runs.record_finish(worker.id, run_id, "success")

    # One count per event list, holding the list so its address stays its own:
    # the memo folds a fresh ``events[covered:]`` slice whenever a list grew
    # since its last fold, and a freed slice's address comes straight back for
    # the next list, which without the hold reads as one list folded twice.
    folds: dict[int, list] = {}
    orig_fold = runs_mod.fold_run_facts
    orig_terminal = runs_mod.terminal_outcome_in_events
    orig_stop = runs_mod.stop_requested_in_events

    def counting_fold(facts, events):
      entry = folds.setdefault(id(events), [events, 0])
      entry[1] += 1
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
    assert max(count for _events, count in folds.values()) == 1
    return max(count for _events, count in folds.values()), questions["n"]

  max_folds_one, _ = await _pass_stats(1)
  max_folds_four, _ = await _pass_stats(4)
  assert max_folds_four == max_folds_one == 1


def test_marker_write_fsyncs_the_temp_file_before_the_rename(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The temp file holds the whole payload, is fsynced, and only then replaces the marker."""
  cfg = config.CharlieBotConfig(charliebot_home=tmp_path / "home")
  started_at = datetime.datetime(2026, 10, 21, 3, 0, 0, 123456, tzinfo=datetime.UTC)
  tmp = _marker_path(cfg).with_suffix(".json.tmp")
  payload = json.dumps({"pass_started_at": started_at.isoformat()})
  steps: list[str] = []

  class _OsStandIn:
    """Stands in for the os module inside task_recovery and records the marker write's steps."""

    def __getattr__(self, name):
      return getattr(os, name)

    def fsync(self, fd):
      steps.append("fsync")
      # The payload is already in the temp file, so the write ran first, and fd is that file.
      assert os.fstat(fd).st_ino == tmp.stat().st_ino
      assert tmp.read_text(encoding="utf-8") == payload
      os.fsync(fd)

    def replace(self, src, dst):
      steps.append("replace")
      assert tmp.read_text(encoding="utf-8") == payload
      os.replace(src, dst)

  monkeypatch.setattr(task_recovery, "os", _OsStandIn())
  task_recovery.write_boot_recovery_marker(cfg, started_at)

  assert steps == ["fsync", "replace"]
  assert _read_marker(cfg) == {"pass_started_at": started_at.isoformat()}
  assert not tmp.exists()
