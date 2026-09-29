"""The task tree's fact-derived archive reaches the session lists.

The sidebar's active list and its Archived list read stored status; a worker
that delivered archives by derivation (archived_of) with no status write. The
task-tree owner registers a read-time overlay on the SessionManager so both
listings, and the list route, see the derived state.
"""

from __future__ import annotations

from pathlib import Path

import orjson
import pytest
from conftest import OPERATOR, build_env

from src.core.models import RunRecord, SessionStatus
from src.core.sessions import SessionManager
from src.core.task_sessions import TaskTreeManager


async def create(tree: TaskTreeManager, *, parent: str | None, request_id: str, profile: str = "manager"):
  return await tree.create_task(
      request_id=request_id,
      task_parent_id=parent,
      profile=profile,
      task=None,
      name=None,
      backend=None,
      caller=OPERATOR)


async def deliver(tree: TaskTreeManager, worker_id: str, run_id: str) -> None:
  """One successful work run: the automatic completion and receipt path."""
  await tree.runs.register_run(RunRecord(id=run_id, session_id=worker_id, kind="work"))
  await tree.dispatch.finish_run(worker_id, run_id, outcome="success")


async def active_ids(session_mgr: SessionManager) -> set[str]:
  return {m.id for m in await session_mgr.list_sessions(status=SessionStatus.ACTIVE)}


async def archived_by_id(session_mgr: SessionManager) -> dict:
  page = await session_mgr.list_archived_page()
  return {m.id: m for m in page["sessions"]}


@pytest.mark.asyncio
async def test_delivered_worker_moves_from_the_active_list_to_the_archived_list(tmp_path: Path) -> None:
  _cfg, session_mgr, tree = build_env(tmp_path)
  root = await create(tree, parent=None, request_id="root")
  worker = await create(tree, parent=root.id, request_id="w1", profile="worker")
  assert await active_ids(session_mgr) == {root.id, worker.id}
  assert await archived_by_id(session_mgr) == {}

  await deliver(tree, worker.id, "run-w1")

  assert tree.task_state(worker.id) == "completed"
  assert await active_ids(session_mgr) == {root.id}
  archived = await archived_by_id(session_mgr)
  assert set(archived) == {worker.id}
  assert archived[worker.id].status == SessionStatus.ARCHIVED
  # Derived, never stored: the metadata on disk still says active.
  stored = await session_mgr.get_session(worker.id)
  assert stored is not None and stored.status == SessionStatus.ACTIVE
  # An unfiltered listing shows the derived state on the row.
  everything = {m.id: m.status for m in await session_mgr.list_sessions()}
  assert everything == {root.id: SessionStatus.ACTIVE, worker.id: SessionStatus.ARCHIVED}


@pytest.mark.asyncio
async def test_failed_worker_stays_in_the_active_list(tmp_path: Path) -> None:
  _cfg, session_mgr, tree = build_env(tmp_path)
  root = await create(tree, parent=None, request_id="root")
  worker = await create(tree, parent=root.id, request_id="w1", profile="worker")
  await tree.runs.register_run(RunRecord(id="run-f", session_id=worker.id, kind="work"))
  await tree.dispatch.finish_run(worker.id, "run-f", outcome="failed")
  await tree.dispatch.deliver_child_report(
      worker.id,
      source_event={"id": "runf-finish"},
      outcome="failed",
      summary="run failed",
      result_refs=[],
      recipient=root.id)

  assert await active_ids(session_mgr) == {root.id, worker.id}
  assert await archived_by_id(session_mgr) == {}


@pytest.mark.asyncio
async def test_hiding_a_parent_archives_its_open_subtree_and_restores_it(tmp_path: Path) -> None:
  cfg, session_mgr, tree = build_env(tmp_path)
  root = await create(tree, parent=None, request_id="root")
  worker = await create(tree, parent=root.id, request_id="w1", profile="worker")
  assert await active_ids(session_mgr) == {root.id, worker.id}
  assert await archived_by_id(session_mgr) == {}

  await tree.set_presentation(root.id, "hidden")

  assert await active_ids(session_mgr) == set()
  archived = await archived_by_id(session_mgr)
  assert set(archived) == {root.id, worker.id}
  assert archived[root.id].status == SessionStatus.ARCHIVED
  # Derived, never stored: the child's metadata on disk still says active/auto.
  stored = orjson.loads((cfg.sessions_dir / worker.id / "metadata.json").read_bytes())
  assert stored["status"] == "active" and stored["presentation"] == "auto"

  await tree.set_presentation(root.id, "auto")
  assert await active_ids(session_mgr) == {root.id, worker.id}
  assert await archived_by_id(session_mgr) == {}
  await tree.set_presentation(root.id, "shown")
  assert await active_ids(session_mgr) == {root.id, worker.id}
  assert await archived_by_id(session_mgr) == {}
