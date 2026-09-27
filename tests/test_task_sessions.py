"""Task-tree owner tests: stable creates, tree validation, derived state, mutation guards."""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from conftest import OPUS_BACKEND_ID, build_env, create_task

from src.core.models import CreateSessionRequest, EventRef, RunRecord
from src.core.task_sessions import (
    TaskInvalidError,
    TaskTreeManager,
)


def write_session_alias(
    path: Path, *, old_session_ids: dict[str, str], old_threads: dict[str, dict] | None = None) -> None:
  """Write one session_aliases.json in the store's own file shape."""
  path.parent.mkdir(parents=True, exist_ok=True)
  path.write_text(
      json.dumps(
          {
              "old_session_ids": old_session_ids,
              "old_threads": old_threads or {},
          },
          ensure_ascii=False,
          indent=2,
          sort_keys=True),
      encoding="utf-8")


async def build_three_levels(mgr: TaskTreeManager) -> dict[str, str]:
  """root(m) -> mid(m) -> low(m) -> {worker1(w), worker2(w)}; returns ids by label."""
  root = await create_task(mgr, parent=None, request_id="root", name="Root")
  mid = await create_task(mgr, parent=root.id, request_id="mid", name="Mid")
  low = await create_task(mgr, parent=mid.id, request_id="low", name="Low")
  worker1 = await create_task(mgr, parent=low.id, request_id="w1", profile="worker", name="W1")
  worker2 = await create_task(mgr, parent=low.id, request_id="w2", profile="worker", name="W2")
  return {"root": root.id, "mid": mid.id, "low": low.id, "worker1": worker1.id, "worker2": worker2.id}


# ---------------------------------------------------------------------------
# Tree shape
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_three_manager_depths_share_one_profile_and_workers_are_leaves(tmp_path: Path) -> None:
  _, session_mgr, mgr = build_env(tmp_path)
  ids = await build_three_levels(mgr)

  for label in ("root", "mid", "low"):
    meta = await session_mgr.get_session(ids[label])
    assert meta is not None and meta.profile == "manager"  # one profile at every manager depth
    assert meta.schema_version == 2

  # A worker is a leaf: no task may be created under it.
  with pytest.raises(TaskInvalidError, match="not a manager"):
    await create_task(mgr, parent=ids["worker1"], profile="worker", request_id="under-worker")


@pytest.mark.asyncio
async def test_history_copying_never_becomes_a_task_parent(tmp_path: Path) -> None:
  cfg, session_mgr, mgr = build_env(tmp_path)
  legacy = await session_mgr.create_session(CreateSessionRequest(name="legacy"), backend=OPUS_BACKEND_ID)
  legacy.parent_session_id = "some-old-session"
  legacy.origin_ref = EventRef(session_id="some-old-session", event_id=None)
  await session_mgr.save_metadata(legacy)

  index = await mgr._get_index()
  assert legacy.id not in index.children.get(None, [])  # not a task-tree node yet
  # A legacy session parents task work in place: the create writes nothing to
  # it (its metadata.json is byte-identical) and the child hangs under it.
  meta_path = cfg.sessions_dir / legacy.id / "metadata.json"
  before = meta_path.read_bytes()
  child = await create_task(mgr, parent=legacy.id, request_id="child-of-legacy")
  assert meta_path.read_bytes() == before
  fresh = await session_mgr.get_session(legacy.id)
  assert fresh is not None and fresh.task_parent_id is None
  assert fresh.profile is None and fresh.schema_version == 1
  assert child.task_parent_id == legacy.id
  index = await mgr._get_index()
  assert legacy.id not in index.children.get(None, [])
  assert child.id in mgr._children_of(index, legacy.id)


# ---------------------------------------------------------------------------
# Stable create / retry

# ---------------------------------------------------------------------------
# Derived state, pagination, deletion
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_permanent_delete_blockers(tmp_path: Path) -> None:
  cfg, _, mgr = build_env(tmp_path)
  ids = await build_three_levels(mgr)

  blockers = await mgr.deletion_blockers(ids["low"])
  assert any("child task" in b for b in blockers)

  await mgr.runs.register_run(RunRecord(id="run-q", session_id=ids["worker1"]))
  blockers = await mgr.deletion_blockers(ids["worker1"])
  assert any("run record" in b for b in blockers)

  triggers_dir = cfg.sessions_dir / ids["worker2"] / "triggers"
  triggers_dir.mkdir(parents=True)
  (triggers_dir / "t.json").write_text("{}", encoding="utf-8")
  blockers = await mgr.deletion_blockers(ids["worker2"])
  assert any("trigger" in b for b in blockers)

  # A saved alias mapping referencing the session blocks deletion too. The
  # file format is the store's contract; the test writes it directly.
  leaf = await create_task(mgr, parent=ids["root"], request_id="leafy", profile="worker", name="Leafy")
  write_session_alias(mgr.aliases.path, old_session_ids={"old-leaf": leaf.id})
  blockers = await mgr.deletion_blockers(leaf.id)
  assert any("alias" in b for b in blockers)
  mgr.aliases.path.unlink()
  assert await mgr.deletion_blockers(leaf.id) == []


# ---------------------------------------------------------------------------
# Mutation guards

# ---------------------------------------------------------------------------
# Prompt bodies
