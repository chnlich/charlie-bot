"""A task-tree node's sidebar activity is the tree's own derivation.

The sidebar row for a task-tree node (profile not None) reads two things from
one shared derivation (:func:`src.core.task_sessions.derive_task_tree_activity`,
the code :meth:`TaskTreeManager.work_state_of` answers from): a
``has_running_tasks`` flag true exactly while one of the node's Runs is live
(recorded pid alive, no terminal fact), and the node's fact-derived
``work_state`` verdict. Every Run fact transition — register, launch, finish,
stop request — marks the node's sidebar state dirty so the next poll reflects
it. Legacy rows never enter this derivation.
"""

from __future__ import annotations

import json
import subprocess
from pathlib import Path

import pytest
import pytest_asyncio
from conftest import (
    _page_request,
    fresh_state_fixture,
    identity_of,
    make_home_session,
)

from src.api import sessions as sessions_api
from src.core import sidebar_state
from src.core.models import RunRecord, TaskSpec
from src.core.run_token import CallerIdentity
from src.core.runs import read_host_boot_time
from src.core.sessions import SessionManager
from src.core.task_sessions import (
    TaskTreeManager,
    derive_task_tree_activity,
)

OP = CallerIdentity(kind="operator")

_clean_sidebar_state = fresh_state_fixture(sidebar_state.reset_for_tests)


@pytest_asyncio.fixture
async def tree_env(tmp_path: Path):
  from src.core.config import CharlieBotConfig

  cfg = CharlieBotConfig(
      charliebot_home=tmp_path / "home",
      backends={"options": [{
          "id": "fake",
          "label": "Fake",
          "type": "cc-claude",
          "model": "fake-model",
      }]},
      paths={"worktree_dir": str(tmp_path / "home" / "worktrees")})
  session_mgr = SessionManager(cfg)
  tree = TaskTreeManager(cfg, session_mgr)
  root = await tree.create_task(
      request_id="root",
      task_parent_id=None,
      profile="manager",
      task=TaskSpec(goal="## Goal\n\nDrive the trial\n"),
      name=None,
      backend=None,
      caller=OP)
  worker = await tree.create_task(
      request_id="w1",
      task_parent_id=root.id,
      profile="worker",
      task=TaskSpec(goal="## Goal\n\nDo the leaf work\n"),
      name=None,
      backend=None,
      caller=OP)
  return tree, session_mgr, root.id, worker.id


async def _register(tree: TaskTreeManager, session_id: str, run_id: str) -> RunRecord:
  return await tree.runs.register_run(RunRecord(id=run_id, session_id=session_id, kind="work"))


# ---------------------------------------------------------------------------
# (a) the shared derivation: (has_running_tasks, work_state) per Run state
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_derivation_reads_queued_run_as_waiting_without_liveness(tree_env) -> None:
  tree, _session_mgr, _root_id, worker_id = tree_env
  await _register(tree, worker_id, "run-1")

  activity = tree.activity_of(worker_id)

  assert (activity.has_running_tasks, activity.work_state) == (False, "waiting")
  # The queued verdict never consults a process: work_state_of is the same
  # derivation's verdict half.
  assert tree.work_state_of(None, worker_id) == "waiting"


@pytest.mark.asyncio
async def test_derivation_reads_live_run_as_running(tree_env) -> None:
  tree, _session_mgr, _root_id, worker_id = tree_env
  run = await _register(tree, worker_id, "run-1")
  proc = subprocess.Popen(["/bin/sleep", "60"])
  try:
    pid, pid_start = identity_of(proc.pid)
    await tree.runs.record_launch(worker_id, run.id, pid=pid, pid_start=pid_start)

    activity = tree.activity_of(worker_id)

    assert (activity.has_running_tasks, activity.work_state) == (True, "running")
  finally:
    if proc.poll() is None:
      proc.kill()


@pytest.mark.asyncio
async def test_derivation_reads_dead_launched_run_as_idle_but_still_blocked(tree_env) -> None:
  """A launched Run whose process died without a terminal fact paints nothing
  (subagent failures are routine), yet stays a structural blocker through
  RunManager.run_blocker — the derivation and the blockers split the job."""
  tree, _session_mgr, _root_id, worker_id = tree_env
  run = await _register(tree, worker_id, "run-1")
  proc = subprocess.Popen(["/bin/sleep", "60"])
  pid, pid_start = identity_of(proc.pid)
  await tree.runs.record_launch(worker_id, run.id, pid=pid, pid_start=pid_start)
  proc.kill()
  proc.wait()

  activity = tree.activity_of(worker_id)

  assert (activity.has_running_tasks, activity.work_state) == (False, "idle")
  stored = tree.runs.read_run_sync(worker_id, run.id)
  assert stored is not None and stored.pid is not None
  events = tree.events.load_events(worker_id)
  assert tree.runs.run_blocker(
      stored, events, read_host_boot_time()) == (f"run {run.id} has an unresolved process identity (needs recovery)")


@pytest.mark.asyncio
async def test_derivation_reads_terminal_facts_without_liveness(tree_env) -> None:
  """A Run with a terminal fact of any outcome contributes nothing: a failed
  Run leaves the node idle, not flagged."""
  tree, _session_mgr, _root_id, worker_id = tree_env
  run = await _register(tree, worker_id, "run-1")
  await tree.runs.record_finish(worker_id, run.id, "failed")

  failed = tree.activity_of(worker_id)
  assert (failed.has_running_tasks, failed.work_state) == (False, "idle")


@pytest.mark.asyncio
async def test_derivation_reads_stopped_queued_run_as_resolved(tree_env) -> None:
  tree, _session_mgr, _root_id, worker_id = tree_env
  run = await _register(tree, worker_id, "run-1")
  await tree.runs.request_stop(worker_id, run.id, "test:stop")

  activity = tree.activity_of(worker_id)

  assert (activity.has_running_tasks, activity.work_state) == (False, "idle")


def test_derivation_is_pure_over_its_inputs() -> None:
  """The shared function answers from (runs, events) alone: no manager, no index."""
  from datetime import UTC, datetime

  run = RunRecord(id="r1", session_id="s1", kind="work")
  activity = derive_task_tree_activity([run], [], lambda: datetime.now(UTC))
  assert (activity.has_running_tasks, activity.work_state) == (False, "waiting")


# ---------------------------------------------------------------------------
# (b) dirty marking on every Run fact transition
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_register_marks_sidebar_dirty(tree_env) -> None:
  tree, _session_mgr, _root_id, worker_id = tree_env
  sidebar_state.reset_for_tests()

  await _register(tree, worker_id, "run-1")

  assert sidebar_state.is_dirty(worker_id)


@pytest.mark.asyncio
async def test_launch_marks_sidebar_dirty(tree_env) -> None:
  tree, _session_mgr, _root_id, worker_id = tree_env
  run = await _register(tree, worker_id, "run-1")
  proc = subprocess.Popen(["/bin/sleep", "60"])
  try:
    pid, pid_start = identity_of(proc.pid)
    sidebar_state.reset_for_tests()

    await tree.runs.record_launch(worker_id, run.id, pid=pid, pid_start=pid_start)

    assert sidebar_state.is_dirty(worker_id)
  finally:
    if proc.poll() is None:
      proc.kill()


@pytest.mark.asyncio
async def test_finish_marks_sidebar_dirty(tree_env) -> None:
  tree, _session_mgr, _root_id, worker_id = tree_env
  run = await _register(tree, worker_id, "run-1")
  sidebar_state.reset_for_tests()

  await tree.runs.record_finish(worker_id, run.id, "failed")

  assert sidebar_state.is_dirty(worker_id)


@pytest.mark.asyncio
async def test_locked_finish_marks_sidebar_dirty(tree_env) -> None:
  tree, _session_mgr, _root_id, worker_id = tree_env
  run = await _register(tree, worker_id, "run-1")
  sidebar_state.reset_for_tests()

  async with tree.control_lock:
    await tree.runs.record_finish_locked(worker_id, run.id, "success")

  assert sidebar_state.is_dirty(worker_id)


@pytest.mark.asyncio
async def test_stop_request_marks_sidebar_dirty(tree_env) -> None:
  tree, _session_mgr, _root_id, worker_id = tree_env
  run = await _register(tree, worker_id, "run-1")
  sidebar_state.reset_for_tests()

  await tree.runs.request_stop(worker_id, run.id, "test:stop")

  assert sidebar_state.is_dirty(worker_id)


# ---------------------------------------------------------------------------
# (c) the status payload: task-tree rows carry the derivation; legacy rows
# keep today's key set byte for byte
# ---------------------------------------------------------------------------


async def _status_json(session_mgr: SessionManager, ids: str) -> dict:
  return json.loads((await sessions_api.all_sessions_status(_page_request(), ids=ids, session_mgr=session_mgr)).body)


@pytest.mark.asyncio
async def test_status_payload_carries_the_task_tree_derivation(tree_env) -> None:
  tree, session_mgr, root_id, worker_id = tree_env
  run = await _register(tree, worker_id, "run-1")
  proc = subprocess.Popen(["/bin/sleep", "60"])
  try:
    pid, pid_start = identity_of(proc.pid)
    await tree.runs.record_launch(worker_id, run.id, pid=pid, pid_start=pid_start)

    payload = await _status_json(session_mgr, ids=f"{worker_id},{root_id}")

    assert payload[worker_id]["has_running_tasks"] is True
    assert payload[worker_id]["work_state"] == "running"
    # The collapsed root's own row stays idle; the stand-in is client-side.
    assert payload[root_id]["has_running_tasks"] is False
    assert payload[root_id]["work_state"] == "idle"
  finally:
    if proc.poll() is None:
      proc.kill()

  await tree.runs.record_finish(worker_id, run.id, "success")
  payload = await _status_json(session_mgr, ids=worker_id)
  assert payload[worker_id]["has_running_tasks"] is False
  assert payload[worker_id]["work_state"] == "idle"


@pytest.mark.asyncio
async def test_legacy_status_payload_keeps_todays_key_set(tmp_path: Path) -> None:
  _cfg, session_mgr, session = await make_home_session(tmp_path, name="Legacy")

  payload = await _status_json(session_mgr, ids=session.id)

  assert set(payload[session.id]) == {
      "has_unread",
      "has_running_tasks",
      "thinking_since",
      "has_pending_trigger",
      "pending_trigger_count",
      "next_trigger_at",
      "has_pending_plan_approval",
  }


@pytest.mark.asyncio
async def test_list_rows_carry_work_state_only_for_task_nodes(tmp_path: Path) -> None:
  from src.core.config import CharlieBotConfig
  from src.core.models import CreateSessionRequest

  cfg = CharlieBotConfig(
      charliebot_home=tmp_path / "home",
      backends={"options": [{
          "id": "fake",
          "label": "Fake",
          "type": "cc-claude",
          "model": "fake-model",
      }]})
  session_mgr = SessionManager(cfg)
  tree = TaskTreeManager(cfg, session_mgr)
  legacy = await session_mgr.create_session(CreateSessionRequest(name="Legacy"))
  node = await tree.create_task(
      request_id="n1",
      task_parent_id=None,
      profile="manager",
      task=TaskSpec(goal="## Goal\n\nNamed node\n"),
      name=None,
      backend=None,
      caller=OP)

  rows = await session_mgr.list_sessions(
      status=None, scheduled=False, include_running_status=True, include_pending_trigger_status=False)
  by_id = {row.id: row for row in rows}
  assert by_id[node.id].work_state == "idle"
  assert by_id[node.id].has_running_tasks is False
  assert by_id[legacy.id].work_state is None


# ---------------------------------------------------------------------------
# (c2) the list wire: GET /api/sessions/ and the search route carry the same
# derivation's verdict for task-tree rows, so the first paint shows the icons
# without waiting for a poll; legacy rows keep their byte-identical key set
# ---------------------------------------------------------------------------


async def _list_json(session_mgr: SessionManager, cfg, thread_mgr) -> list[dict]:
  body = await sessions_api.list_sessions(_page_request(), session_mgr=session_mgr, cfg=cfg, thread_mgr=thread_mgr)
  return json.loads(body.body)


@pytest.mark.asyncio
async def test_list_rows_carry_work_state_for_task_nodes_and_null_for_legacy(tree_env, tmp_path: Path) -> None:
  from src.core.models import CreateSessionRequest
  from src.core.threads import ThreadManager

  tree, session_mgr, root_id, worker_id = tree_env
  cfg = tree.cfg
  run = await _register(tree, worker_id, "run-1")
  proc = subprocess.Popen(["/bin/sleep", "60"])
  try:
    pid, pid_start = identity_of(proc.pid)
    await tree.runs.record_launch(worker_id, run.id, pid=pid, pid_start=pid_start)

    rows = {row["id"]: row for row in await _list_json(session_mgr, cfg, ThreadManager(cfg))}

    # The running worker and its idle root read the same verdict the status
    # payload serves — one derivation, two surfaces.
    assert rows[worker_id]["work_state"] == "running"
    assert rows[root_id]["work_state"] == "idle"
    status = await _status_json(session_mgr, ids=f"{worker_id},{root_id}")
    assert rows[worker_id]["work_state"] == status[worker_id]["work_state"]
    assert rows[root_id]["work_state"] == status[root_id]["work_state"]
  finally:
    if proc.poll() is None:
      proc.kill()

  await tree.runs.record_finish(worker_id, run.id, "failed")
  rows = {row["id"]: row for row in await _list_json(session_mgr, cfg, ThreadManager(cfg))}
  # A terminal fact of any outcome settles the Run: the wire reads idle.
  assert rows[worker_id]["work_state"] == "idle"

  # A legacy row's wire body carries the field's null exactly as before: no
  # derived verdict ever lands on it.
  legacy = await session_mgr.create_session(CreateSessionRequest(name="Legacy"))
  rows = {row["id"]: row for row in await _list_json(session_mgr, cfg, ThreadManager(cfg))}
  assert rows[legacy.id]["work_state"] is None


@pytest.mark.asyncio
async def test_search_rows_carry_work_state_for_task_nodes(tree_env) -> None:
  from src.core.threads import ThreadManager

  tree, session_mgr, root_id, worker_id = tree_env
  cfg = tree.cfg
  await _register(tree, worker_id, "run-1")  # queued -> waiting

  body = await sessions_api.search_sessions(
      _page_request(), q=" ", session_mgr=session_mgr, cfg=cfg, thread_mgr=ThreadManager(cfg))
  rows = {row["id"]: row for row in json.loads(body.body)}
  assert rows[worker_id]["work_state"] == "waiting"
  assert rows[root_id]["work_state"] == "idle"


# ---------------------------------------------------------------------------
# (d) names: nodes take goal-derived names (the rule itself is origin/main's,
# pinned in tests/test_task_sessions.py)
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_created_nodes_take_goal_derived_names(tree_env) -> None:
  tree, _session_mgr, root_id, worker_id = tree_env
  index = await tree._get_index()
  assert index.metas[root_id].name == "Drive the trial"
  assert index.metas[worker_id].name == "Do the leaf work"
