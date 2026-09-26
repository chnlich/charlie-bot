"""The unread-reply flag reaches the task tree's rows as an independent fact.

The tree rows render the sidebar's unread dot from ``SessionRow.has_unread``.
That flag is the SessionManager-owned unread state (``mark_unread`` when a
reply is delivered, ``mark_read`` on opening) and is deliberately independent
of ``work_state``: an idle task can carry an unread reply, a failed or
cancelled Run must not look like one, and an active Run hides the dot without
discarding the flag. These tests pin the flag on the real projection, the
invalidation seam that keeps a post-flip tree page fresh, and the fact that
reading one task never touches another.
"""

from __future__ import annotations

from pathlib import Path

import pytest
import pytest_asyncio
from conftest import live_subprocess, make_home_config

from src.core import event_types as ET
from src.core.models import RunRecord
from src.core.run_token import CallerIdentity
from src.core.runs import read_pid_stat
from src.core.sessions import SessionManager
from src.core.task_sessions import TaskTreeManager

OP = CallerIdentity(kind="operator")


@pytest_asyncio.fixture
async def env(tmp_path: Path):
  cfg = make_home_config(tmp_path)
  session_mgr = SessionManager(cfg)
  tree = TaskTreeManager(cfg, session_mgr)
  root = await tree.create_task(
      request_id="root", task_parent_id=None, profile="manager", task=None, name="Root", backend=None, caller=OP)
  worker_a = await tree.create_task(
      request_id="wa", task_parent_id=root.id, profile="worker", task=None, name="Worker A", backend=None, caller=OP)
  worker_b = await tree.create_task(
      request_id="wb", task_parent_id=root.id, profile="worker", task=None, name="Worker B", backend=None, caller=OP)
  return tree, session_mgr, root.id, worker_a.id, worker_b.id


def row_of(page: dict, session_id: str) -> dict:
  return next(r for r in page["items"] if r["id"] == session_id)


@pytest.mark.asyncio
async def test_tree_rows_carry_the_unread_flag_and_follow_the_writer(env) -> None:
  tree, session_mgr, root_id, worker_a_id, _worker_b_id = env
  page = await tree.tree_page(parent_id=root_id, include_archived=False, limit=10, cursor=None)
  row = row_of(page, worker_a_id)
  assert row["has_unread"] is False, "a fresh task starts read"
  assert row["work_state"] == "idle"

  # Force-build the index immediately before the flip, so the freshness this
  # test pins is the invalidation seam, not the index TTL happening to expire.
  await tree._get_index()
  await session_mgr.mark_unread(worker_a_id)

  page = await tree.tree_page(parent_id=root_id, include_archived=False, limit=10, cursor=None)
  assert row_of(page, worker_a_id)["has_unread"] is True, (
      "a tree page read after the flip serves the fresh flag: no TTL wait, no reload")
  root_page = await tree.tree_page(parent_id=None, include_archived=False, limit=10, cursor=None)
  assert row_of(root_page, root_id)["has_unread"] is False, "only the flipped node carries the flag"

  await session_mgr.mark_read(worker_a_id)
  page = await tree.tree_page(parent_id=root_id, include_archived=False, limit=10, cursor=None)
  assert row_of(page, worker_a_id)["has_unread"] is False, "the existing mark-read path clears the flag"


@pytest.mark.asyncio
async def test_unread_is_independent_of_work_state_across_a_run(env) -> None:
  tree, session_mgr, _root_id, worker_a_id, _worker_b_id = env
  run = await tree.runs.register_run(RunRecord(id="run-1", session_id=worker_a_id, kind="work"))
  proc = live_subprocess()
  try:
    pair = read_pid_stat(proc.pid)
    assert pair is not None
    await tree.runs.record_launch(worker_a_id, run.id, pid=proc.pid, pid_start=pair[0])
    index = await tree._get_index()
    running_row = tree.session_row(index, worker_a_id)
    assert running_row.work_state == "running"
    await session_mgr.mark_unread(worker_a_id)
    index = await tree._get_index()
    row = tree.session_row(index, worker_a_id)
    assert row.work_state == "running" and row.has_unread is True, (
        "an active run hides the dot without discarding the unread flag")

    finished = await tree.dispatch.finish_run(worker_a_id, run.id, outcome="success", exit_code=0)
    assert finished.ended_at is not None
    index = await tree._get_index()
    row = tree.session_row(index, worker_a_id)
    assert row.work_state == "idle", "a task that stays open after its Run finishes is idle"
    assert row.has_unread is True, (
        "the terminal fact must not clear unread: the reply waits behind the activity, then shows")
  finally:
    if proc.poll() is None:
      proc.kill()
  await session_mgr.mark_read(worker_a_id)
  index = await tree._get_index()
  row = tree.session_row(index, worker_a_id)
  assert row.work_state == "idle" and row.has_unread is False, "opening the task clears the dot"


@pytest.mark.asyncio
async def test_terminal_states_never_infer_unread(env) -> None:
  tree, _session_mgr, _root_id, worker_a_id, _worker_b_id = env
  # A successful run ends idle, still not unread.
  run_ok = await tree.runs.register_run(RunRecord(id="run-o", session_id=worker_a_id, kind="work"))
  await tree.dispatch.finish_run(worker_a_id, run_ok.id, outcome="success", exit_code=0)
  index = await tree._get_index()
  row = tree.session_row(index, worker_a_id)
  assert row.work_state == "idle" and row.has_unread is False, ("success alone is not an unread reply")

  # A failed run reads idle, not an unread dot.
  run_failed = await tree.runs.register_run(RunRecord(id="run-f", session_id=worker_a_id, kind="work"))
  await tree.dispatch.finish_run(worker_a_id, run_failed.id, outcome="failed", exit_code=1)
  index = await tree._get_index()
  row = tree.session_row(index, worker_a_id)
  assert row.work_state == "idle" and row.has_unread is False, (
      "a failed Run is idle; failure alone is not an unread reply")

  # A stopped run's terminal fact settles it idle, still not unread.
  run_stopped = await tree.runs.register_run(RunRecord(id="run-s", session_id=worker_a_id, kind="work"))
  proc = live_subprocess()
  try:
    pair = read_pid_stat(proc.pid)
    assert pair is not None
    await tree.runs.record_launch(worker_a_id, run_stopped.id, pid=proc.pid, pid_start=pair[0])
    result = await tree.runs.request_stop(worker_a_id, run_stopped.id, request_id="stop-1")
    assert result.stop_requested is True and result.outcome == "interrupted"
  finally:
    if proc.poll() is None:
      proc.kill()
  index = await tree._get_index()
  row = tree.session_row(index, worker_a_id)
  assert row.work_state == "idle" and row.has_unread is False, ("a cancelled/stopped Run is not automatically unread")


@pytest.mark.asyncio
async def test_mark_read_leaves_other_sessions_unread(env, monkeypatch: pytest.MonkeyPatch) -> None:
  tree, session_mgr, root_id, worker_a_id, worker_b_id = env
  await session_mgr.mark_unread(worker_a_id)
  await session_mgr.mark_unread(worker_b_id)

  unread_broadcasts: list[dict] = []
  tree_broadcasts: list[tuple[str, str | None]] = []

  async def spy_sidebar(session_id: str, event_type: str, **fields) -> None:
    if event_type == ET.UNREAD_CHANGED:
      unread_broadcasts.append({"session_id": session_id, **fields})

  async def spy_tree(session_id: str, event_type: str | None) -> None:
    tree_broadcasts.append((session_id, event_type))

  monkeypatch.setattr(session_mgr, "_broadcast_sidebar", spy_sidebar)
  monkeypatch.setattr(session_mgr, "broadcast_task_tree_changed", spy_tree)

  await session_mgr.mark_read(worker_a_id)
  page = await tree.tree_page(parent_id=root_id, include_archived=False, limit=10, cursor=None)
  assert row_of(page, worker_a_id)["has_unread"] is False
  assert row_of(page, worker_b_id)["has_unread"] is True, ("reading one task never marks another one read")

  # The flip rides the sidebar unread channel only, with the flip's own value:
  # a read/unread change never triggers a tree refresh, and no tree
  # notification is emitted for the untouched sibling.
  assert unread_broadcasts == [{"session_id": worker_a_id, "has_unread": False}]
  assert tree_broadcasts == []


@pytest.mark.asyncio
async def test_unread_flip_does_not_move_the_tree_revision(env) -> None:
  tree, session_mgr, _root_id, worker_a_id, _worker_b_id = env
  before = await tree.tree_page(parent_id=None, include_archived=False, limit=10, cursor=None)
  await session_mgr.mark_unread(worker_a_id)
  after = await tree.tree_page(parent_id=None, include_archived=False, limit=10, cursor=None)
  assert before["tree_revision"] == after["tree_revision"], (
      "an unread flip is not a structural change: pagination cursors stay valid")
