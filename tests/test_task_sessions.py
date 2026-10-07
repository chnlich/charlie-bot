"""Task-tree owner tests: stable creates, tree validation, derived state, mutation guards."""

from __future__ import annotations

import datetime
import json
import pathlib

import conftest
import pytest

from src.infra import models
from src.runtime import task_sessions


def write_session_alias(
    path: pathlib.Path, *, old_session_ids: dict[str, str], old_threads: dict[str, dict] | None = None) -> None:
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


async def build_three_levels(mgr: task_sessions.TaskTreeManager) -> dict[str, str]:
  """root(m) -> mid(m) -> low(m) -> {worker1(w), worker2(w)}; returns ids by label."""
  root = await conftest.create_task(mgr, parent=None, request_id="root", name="Root")
  mid = await conftest.create_task(mgr, parent=root.id, request_id="mid", name="Mid")
  low = await conftest.create_task(mgr, parent=mid.id, request_id="low", name="Low")
  worker1 = await conftest.create_task(mgr, parent=low.id, request_id="w1", profile="worker", name="W1")
  worker2 = await conftest.create_task(mgr, parent=low.id, request_id="w2", profile="worker", name="W2")
  return {"root": root.id, "mid": mid.id, "low": low.id, "worker1": worker1.id, "worker2": worker2.id}


# ---------------------------------------------------------------------------
# Tree shape
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_three_manager_depths_share_one_profile_and_workers_are_leaves(tmp_path: pathlib.Path) -> None:
  _, session_mgr, mgr = conftest.build_env(tmp_path)
  ids = await build_three_levels(mgr)

  for label in ("root", "mid", "low"):
    meta = await session_mgr.get_session(ids[label])
    assert meta is not None and meta.profile == "manager"  # one profile at every manager depth
    assert meta.schema_version == 2

  # A worker is a leaf: no task may be created under it.
  with pytest.raises(task_sessions.TaskInvalidError, match="not a manager"):
    await conftest.create_task(mgr, parent=ids["worker1"], profile="worker", request_id="under-worker")


@pytest.mark.asyncio
async def test_history_copying_never_becomes_a_task_parent(tmp_path: pathlib.Path) -> None:
  cfg, session_mgr, mgr = conftest.build_env(tmp_path)
  legacy = await session_mgr.create_session(
      models.CreateSessionRequest(name="legacy"), backend=conftest.OPUS_BACKEND_ID)
  legacy.parent_session_id = "some-old-session"
  legacy.origin_ref = models.EventRef(session_id="some-old-session", event_id=None)
  await session_mgr.save_metadata(legacy)

  index = await mgr._get_index()
  assert legacy.id not in index.children.get(None, [])  # not a task-tree node yet
  # A legacy session parents task work in place: the create writes nothing to
  # it (its metadata.json is byte-identical) and the child hangs under it.
  meta_path = cfg.sessions_dir / legacy.id / "metadata.json"
  before = meta_path.read_bytes()
  child = await conftest.create_task(mgr, parent=legacy.id, request_id="child-of-legacy")
  assert meta_path.read_bytes() == before
  fresh = await session_mgr.get_session(legacy.id)
  assert fresh is not None and fresh.task_parent_id is None
  assert fresh.profile is None and fresh.schema_version == 1
  assert child.task_parent_id == legacy.id
  index = await mgr._get_index()
  assert legacy.id not in index.children.get(None, [])
  assert child.id in mgr._children_of(index, legacy.id)


# ---------------------------------------------------------------------------
# Derived state, pagination, deletion
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_permanent_delete_blockers(tmp_path: pathlib.Path) -> None:
  cfg, _, mgr = conftest.build_env(tmp_path)
  ids = await build_three_levels(mgr)

  blockers = await mgr.deletion_blockers(ids["low"])
  assert any("child task" in b for b in blockers)

  await mgr.runs.register_run(models.RunRecord(id="run-q", session_id=ids["worker1"]))
  blockers = await mgr.deletion_blockers(ids["worker1"])
  assert any("run record" in b for b in blockers)

  triggers_dir = cfg.sessions_dir / ids["worker2"] / "triggers"
  triggers_dir.mkdir(parents=True)
  (triggers_dir / "t.json").write_text("{}", encoding="utf-8")
  blockers = await mgr.deletion_blockers(ids["worker2"])
  assert any("trigger" in b for b in blockers)

  # A saved alias mapping referencing the session blocks deletion too. The
  # file format is the store's contract; the test writes it directly.
  leaf = await conftest.create_task(mgr, parent=ids["root"], request_id="leafy", profile="worker", name="Leafy")
  write_session_alias(mgr.aliases.path, old_session_ids={"old-leaf": leaf.id})
  blockers = await mgr.deletion_blockers(leaf.id)
  assert any("alias" in b for b in blockers)
  mgr.aliases.path.unlink()
  assert await mgr.deletion_blockers(leaf.id) == []


# ---------------------------------------------------------------------------
# Scheduler bookkeeping
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_scheduled_fire_bookkeeping_keeps_the_sidebar_sort_key(tmp_path: pathlib.Path) -> None:
  """Every record_scheduled_fire call shape the scheduler uses writes its
  scheduling fields and leaves updated_at as it was: a frequent cron's node
  keeps its sidebar place, while a listing read shows the new Last status."""
  _, session_mgr, tree = conftest.build_env(tmp_path)
  node = await conftest.create_scheduled_node(tree, name="nightly", backend=conftest.OPUS_BACKEND_ID)
  fired_at = datetime.datetime(2026, 1, 2, 3, 4, 5, tzinfo=datetime.UTC)
  await session_mgr.update_thinking_state(node.id, fired_at)

  # The five call shapes src/features/cron/scheduler.py fires with, and the metadata
  # fields each must land (the scheduler's cron argument writes
  # last_scheduled_cron): cron change, overlap skip, normal fire, loop noop,
  # handler outcome.
  stamp = fired_at.isoformat()
  cron_call = {"last_scheduled_run": stamp, "cron": "0 3 * * *"}
  cron_land = {"last_scheduled_run": stamp, "last_scheduled_cron": "0 3 * * *"}
  skip_call = {"last_scheduled_run": stamp, "last_run_status": models.LastRunStatus.SKIPPED}
  noop = {"last_run_status": models.LastRunStatus.SUCCESS}
  handler_failed = {"last_run_status": models.LastRunStatus.FAILED}
  shapes = [
      (cron_call, cron_land),
      (skip_call, skip_call),
      (cron_call, cron_land),
      (noop, noop),
      (handler_failed, handler_failed),
  ]
  for call, landed in shapes:
    meta = await tree.record_scheduled_fire(node.id, **call)
    assert meta.updated_at == fired_at
    fresh = await session_mgr.get_session(node.id)
    assert fresh is not None and fresh.updated_at == fired_at
    row = next(r for r in await session_mgr.list_sessions(status=models.SessionStatus.ACTIVE) if r.id == node.id)
    for name, value in landed.items():
      assert getattr(row, name) == value
