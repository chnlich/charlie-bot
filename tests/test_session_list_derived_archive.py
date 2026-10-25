"""The task tree's fact-derived archive reaches the session lists.

The sidebar's active list and its Archived list read stored status; a worker
that delivered archives by derivation (archived_of) with no status write. The
task-tree owner registers a read-time overlay on the listing block so both
listings, and the list route, see the derived state.
"""

from __future__ import annotations

import pathlib

import conftest
import orjson
import pytest

from src.infra import models
from src.runtime import task_errors, task_sessions


async def deliver(tree: task_sessions.TaskTreeManager, worker_id: str, run_id: str) -> None:
  """One successful work run: the automatic completion and receipt path."""
  await tree.runs.register_run_locked(models.RunRecord(id=run_id, session_id=worker_id, kind="work"))
  await tree.dispatch.finish_run(worker_id, run_id, outcome="success")


async def active_ids(session_blocks: conftest.SessionBlocks) -> set[str]:
  return {m.id for m in await session_blocks.listing.list_sessions(status=models.SessionStatus.ACTIVE)}


async def archived_by_id(session_blocks: conftest.SessionBlocks) -> dict:
  page = await session_blocks.listing.list_archived_page()
  return {m.id: m for m in page["sessions"]}


@pytest.mark.asyncio
async def test_delivered_worker_moves_from_the_active_list_to_the_archived_list(tmp_path: pathlib.Path) -> None:
  _cfg, session_blocks, tree = conftest.build_env(tmp_path)
  root = await conftest.create_task(tree, parent=None, request_id="root")
  worker = await conftest.create_task(tree, parent=root.id, request_id="w1", profile="worker")
  assert await active_ids(session_blocks) == {root.id, worker.id}
  assert await archived_by_id(session_blocks) == {}

  await deliver(tree, worker.id, "run-w1")

  assert tree.task_state(worker.id) == "completed"
  assert await active_ids(session_blocks) == {root.id}
  archived = await archived_by_id(session_blocks)
  assert set(archived) == {worker.id}
  assert archived[worker.id].status == models.SessionStatus.ARCHIVED
  # Derived, never stored: the metadata on disk still says active.
  stored = await session_blocks.store.get_session(worker.id)
  assert stored is not None and stored.status == models.SessionStatus.ACTIVE
  # An unfiltered listing shows the derived state on the row.
  everything = {m.id: m.status for m in await session_blocks.listing.list_sessions()}
  assert everything == {root.id: models.SessionStatus.ACTIVE, worker.id: models.SessionStatus.ARCHIVED}


@pytest.mark.asyncio
async def test_failed_worker_stays_in_the_active_list(tmp_path: pathlib.Path) -> None:
  _cfg, session_blocks, tree = conftest.build_env(tmp_path)
  root = await conftest.create_task(tree, parent=None, request_id="root")
  worker = await conftest.create_task(tree, parent=root.id, request_id="w1", profile="worker")
  await tree.runs.register_run_locked(models.RunRecord(id="run-f", session_id=worker.id, kind="work"))
  await tree.dispatch.finish_run(worker.id, "run-f", outcome="failed")
  await tree.dispatch.deliver_child_report(
      worker.id,
      source_event={"id": "runf-finish"},
      outcome="failed",
      summary="run failed",
      result_refs=[],
      recipient=root.id)

  assert await active_ids(session_blocks) == {root.id, worker.id}
  assert await archived_by_id(session_blocks) == {}


@pytest.mark.asyncio
async def test_archiving_a_parent_archives_its_open_subtree_and_restore_brings_it_back(tmp_path: pathlib.Path) -> None:
  cfg, session_blocks, tree = conftest.build_env(tmp_path)
  root = await conftest.create_task(tree, parent=None, request_id="root")
  worker = await conftest.create_task(tree, parent=root.id, request_id="w1", profile="worker")
  assert await active_ids(session_blocks) == {root.id, worker.id}
  assert await archived_by_id(session_blocks) == {}

  assert await tree.completion.archive_subtree(
      root.id,
      request_id="archive-test",
      summary="archived by the user",
      result_refs=[],
      run_ids=[],
      caller=conftest.OPERATOR) == [root.id, worker.id]

  assert await active_ids(session_blocks) == set()
  archived = await archived_by_id(session_blocks)
  assert set(archived) == {root.id, worker.id}
  assert archived[root.id].status == models.SessionStatus.ARCHIVED
  # Derived, never stored: the child's metadata on disk still says active,
  # and no status write ever happened (the close facts are the archive).
  stored = orjson.loads((cfg.sessions_dir / worker.id / "metadata.json").read_bytes())
  assert stored["status"] == "active" and "presentation" not in stored

  restored = await tree.completion.restore_chain(worker.id, request_id="sidebar-1", reason="sidebar unarchive")
  assert restored == [root.id, worker.id]  # the whole archived chain, topmost first
  assert await active_ids(session_blocks) == {root.id, worker.id}
  assert await archived_by_id(session_blocks) == {}
  # The restore moved only the chain's nodes: facts, not status writes.
  for sid in (root.id, worker.id):
    assert tree.task_state(sid) == "open"
    stored = orjson.loads((cfg.sessions_dir / sid / "metadata.json").read_bytes())
    assert stored["status"] == "active"


def bind_in_memory_tree(
    monkeypatch: pytest.MonkeyPatch,
    tree: task_sessions.TaskTreeManager,
    edges: list[tuple[str, str | None]],
    closed: frozenset[str] = frozenset(),
    stored_archived: frozenset[str] = frozenset()
) -> None:
  """Serve *tree* an index built from (id, task parent) pairs.

  The *closed* ids' task folds to completed and the *stored_archived* ids carry the archived status. Creating
  thousands of sessions on disk would blow the unit budget, and the walk reads only the index and the task
  facts, so both are supplied in memory.
  """
  metas: dict[str, models.SessionMetadata] = {}
  children: dict[str | None, list[str]] = {}
  for sid, parent in edges:
    status = models.SessionStatus.ARCHIVED if sid in stored_archived else models.SessionStatus.ACTIVE
    metas[sid] = models.SessionMetadata(id=sid, name=sid, profile="worker", task_parent_id=parent, status=status)
    children.setdefault(parent, []).append(sid)
  index = task_sessions._TreeIndex(metas=metas, children=children, revision="", root_sig=(0, 0))

  async def get_index(*, force: bool = False) -> task_sessions._TreeIndex:
    return index

  def task_state_at(session_id: str, identity: tuple[int, int, int] | None) -> str:
    return "completed" if session_id in closed else "open"

  monkeypatch.setattr(tree, "_get_index", get_index)
  monkeypatch.setattr(tree, "_task_state_at", task_state_at)


@pytest.mark.asyncio
async def test_closed_descendants_are_returned_and_do_not_count_toward_the_cap(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  _cfg, _session_blocks, tree = conftest.build_env(tmp_path)
  kids = [f"k{i}" for i in range(1100)]
  bind_in_memory_tree(monkeypatch, tree, [("root", None), *[(k, "root") for k in kids]], closed=frozenset(kids[:1050]))

  descendants = await tree.descendant_ids("root")

  assert len(descendants) == 1100 and set(descendants) == set(kids)


@pytest.mark.asyncio
async def test_open_descendants_past_the_cap_raise(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  _cfg, _session_blocks, tree = conftest.build_env(tmp_path)
  bind_in_memory_tree(monkeypatch, tree, [("root", None), *[(f"k{i}", "root") for i in range(1001)]])

  with pytest.raises(task_errors.TaskConflictError) as raised:
    await tree.descendant_ids("root")

  assert raised.value.blockers == ["subtree of root exceeds 1000 non-archived nodes"]


@pytest.mark.asyncio
async def test_exactly_the_cap_of_open_descendants_is_allowed(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  _cfg, _session_blocks, tree = conftest.build_env(tmp_path)
  # 50 stored-archived nodes are walked but not counted: only the 1000 open ones meet the cap.
  stored_archived = [f"a{i}" for i in range(50)]
  edges = [("root", None), *[(f"k{i}", "root") for i in range(1000)], *[(a, "root") for a in stored_archived]]
  bind_in_memory_tree(monkeypatch, tree, edges, stored_archived=frozenset(stored_archived))

  descendants = await tree.descendant_ids("root")

  assert len(descendants) == 1050


@pytest.mark.asyncio
async def test_open_nodes_under_a_closed_ancestor_count_as_archived(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  _cfg, _session_blocks, tree = conftest.build_env(tmp_path)
  under_closed = [f"o{i}" for i in range(1500)]
  bind_in_memory_tree(
      monkeypatch,
      tree, [("root", None), ("closed", "root"), *[(o, "closed") for o in under_closed]],
      closed=frozenset({"closed"}))

  descendants = await tree.descendant_ids("root")

  assert len(descendants) == 1501 and set(under_closed) <= set(descendants)
