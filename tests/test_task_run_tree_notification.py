"""Run lifecycle facts reach the tree's connected clients through one seam.

The sidebar task tree renders activity from SessionRow facts (work_state,
running_descendant_count). Those facts move on durable Run writes; the
notification seam is the ControlEventSink's best-effort tree broadcast. A Run
transition whose write lands without a notification leaves every connected
tree stale until an unrelated later fact: the launch (queued -> running) was
exactly that gap. These tests pin the seam for launch and the terminal
outcomes, and the ancestor activity count the collapsed-row cue reads.
"""

from __future__ import annotations

from pathlib import Path

import pytest
import pytest_asyncio
from conftest import OPERATOR, NotificationSpy, build_env, identity_of, live_subprocess

from src.core import event_types as ET
from src.core.models import RunRecord


@pytest_asyncio.fixture
async def env(tmp_path: Path):
  _, session_mgr, tree = build_env(tmp_path)
  root = await tree.create_task(
      request_id="root", task_parent_id=None, profile="manager", task=None, name="Root", backend=None, caller=OPERATOR)
  worker = await tree.create_task(
      request_id="w1",
      task_parent_id=root.id,
      profile="worker",
      task=None,
      name="Worker",
      backend=None,
      caller=OPERATOR)
  return tree, session_mgr, root.id, worker.id


@pytest.mark.asyncio
async def test_record_launch_notifies_with_running_rows_readable_at_signal(env) -> None:
  tree, _session_mgr, root_id, worker_id = env
  run = await tree.runs.register_run(RunRecord(id="run-1", session_id=worker_id, kind="work"))
  # Queued first: the row is waiting work and no ancestor shows delegated running.
  index = await tree._get_index()
  assert tree.session_row(index, worker_id).work_state == "waiting"
  assert tree.session_row(index, root_id).running_descendant_count == 0

  spy = NotificationSpy(tree)
  spy.install()
  proc = live_subprocess()
  try:
    pid, pid_start = identity_of(proc.pid)
    await tree.runs.record_launch(worker_id, run.id, pid=pid, pid_start=pid_start)

    assert spy.calls == [(worker_id, "run_launched")
                        ], ("the launch lands as one tree notification for the launched node")
    node = spy.rows_at_signal[0]["node"]
    assert node["id"] == worker_id and node["work_state"] == "running", (
        "at signal time the tree projection already reads the launch: no further event or reload needed")
    root_row = tree.session_row(await tree._get_index(), root_id)
    assert root_row.running_descendant_count == 1, ("the collapsed-ancestor cue sees the running descendant")
  finally:
    if proc.poll() is None:
      proc.kill()


@pytest.mark.asyncio
async def test_terminal_outcomes_notify_and_clear_running_ancestor_counts(env) -> None:
  tree, _session_mgr, root_id, worker_id = env
  run = await tree.runs.register_run(RunRecord(id="run-1", session_id=worker_id, kind="work"))
  proc = live_subprocess()
  try:
    pid, pid_start = identity_of(proc.pid)
    await tree.runs.record_launch(worker_id, run.id, pid=pid, pid_start=pid_start)
  finally:
    if proc.poll() is None:
      proc.kill()

  spy = NotificationSpy(tree)
  spy.install()
  finished = await tree.dispatch.finish_run(worker_id, run.id, outcome="success", exit_code=0)
  assert finished.ended_at is not None
  assert (worker_id, ET.RUN_FINISHED) in spy.calls, ("the terminal fact rides the same durable-fact notification seam")
  node = tree.session_row(await tree._get_index(), worker_id)
  assert node.work_state == "idle", ("a task that stays open after its Run finishes is idle, not perpetually running")
  root_row = tree.session_row(await tree._get_index(), root_id)
  assert root_row.running_descendant_count == 0, ("the collapsed-ancestor cue clears with the terminal fact")

  # A failed run reads idle, not running: a terminal Run is not activity.
  run2 = await tree.runs.register_run(RunRecord(id="run-2", session_id=worker_id, kind="work"))
  await tree.dispatch.finish_run(worker_id, run2.id, outcome="failed", exit_code=1)
  failed_row = tree.session_row(await tree._get_index(), worker_id)
  assert failed_row.work_state == "idle"
  assert tree.session_row(await tree._get_index(), root_id).running_descendant_count == 0
