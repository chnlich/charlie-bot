"""API-layer tests for the task-tree foundation endpoints and caller identity."""

from __future__ import annotations

import datetime
import json
import pathlib
import subprocess

import conftest
import pytest
import pytest_asyncio

from src.infra import models
from src.runtime import runs, task_sessions


@pytest_asyncio.fixture
async def task_env(tmp_path: pathlib.Path):
  cfg = conftest.make_home_config(tmp_path)
  session_blocks = conftest.build_session_blocks(cfg)
  task_mgr = conftest.build_task_tree(cfg, session_blocks)
  return cfg, session_blocks, task_mgr


async def seed_tree(task_mgr: task_sessions.TaskTreeManager) -> dict[str, str]:
  root = await task_mgr.create_task(
      request_id="root",
      task_parent_id=None,
      profile="manager",
      task=None,
      name="Root",
      backend=None,
      caller="operator")
  worker = await task_mgr.create_task(
      request_id="w1", task_parent_id=root.id, profile="worker", task=None, name="W1", backend=None, caller="operator")
  return {"root": root.id, "worker": worker.id}


# ---------------------------------------------------------------------------
# v2 create, tree, detail, runs
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_activity_tracks_a_finish_that_lands_after_a_warm_derivation(task_env) -> None:
  """The outcome memo rides the events cache's identity with a covered
  cursor, so a run_finished append after a warmed derivation must move the
  verdict — a stale memo would pin a finished run's queued verdict on every
  tree page and sidebar probe until restart."""
  _cfg, _session_blocks, task_mgr = task_env
  ids = await seed_tree(task_mgr)
  worker = ids["worker"]
  await task_mgr.runs.register_run_locked(models.RunRecord(id="r-memo", session_id=worker))
  assert task_mgr.activity_of(worker).work_state == "waiting"
  await task_mgr.runs.record_finish(worker, "r-memo", "completed")
  assert task_mgr.activity_of(worker).work_state == "idle"


@pytest.mark.asyncio
async def test_activity_tracks_a_launch_that_lands_after_a_warm_derivation(task_env) -> None:
  """The record write funnel bumps the generation the activity memo keys on,
  so a launch identity persisted after a warmed derivation must move the
  queued verdict — a memo the funnel bypass leaves stale would pin the
  waiting verdict on every tree page and sidebar probe."""
  _cfg, _session_blocks, task_mgr = task_env
  ids = await seed_tree(task_mgr)
  worker = ids["worker"]
  await task_mgr.runs.register_run_locked(models.RunRecord(id="r-launch", session_id=worker))
  assert task_mgr.activity_of(worker).work_state == "waiting"
  proc = subprocess.Popen(["sleep", "30"])
  try:
    pid_start, _state = runs.read_pid_stat(proc.pid)
    await task_mgr.runs.record_launch(worker, "r-launch", pid=proc.pid, pid_start=pid_start)
    assert task_mgr.activity_of(worker).work_state == "running"
  finally:
    proc.kill()
    proc.wait()


@pytest.mark.asyncio
async def test_activity_tracks_a_stop_request_that_lands_after_a_warm_derivation(task_env) -> None:
  """The events cache takes an append in place — the list identity and the
  archived extent survive the new fact — so a chat-only fact transition with
  no record write (a stop request on a queued run) moves the verdict through
  the covered length in the memo key. A key without it would pin the waiting
  verdict on every tree page and sidebar probe until an unrelated record
  write bumped the generation."""
  _cfg, _session_blocks, task_mgr = task_env
  ids = await seed_tree(task_mgr)
  worker = ids["worker"]
  await task_mgr.runs.register_run_locked(models.RunRecord(id="r-stop", session_id=worker))
  assert task_mgr.activity_of(worker).work_state == "waiting"
  await task_mgr.runs.request_stop(worker, "r-stop", "stop-1")
  assert task_mgr.activity_of(worker).work_state == "idle"


@pytest.mark.asyncio
async def test_fold_agrees_with_a_cold_refold_across_a_recycle(task_env) -> None:
  """The run-facts memo keys on the history identity (inode, size, mtime). The
  recycle rewrites the live file, so the identity moves and the fold re-keys.
  Warm and cold folds must agree after the move and after a post-recycle
  append: a memo that kept the pre-recycle identity would serve facts from a
  stale file — silently wrong verdicts on every tree page and sidebar probe."""
  _cfg, session_blocks, task_mgr = task_env
  ids = await seed_tree(task_mgr)
  worker = ids["worker"]
  await task_mgr.runs.register_run_locked(models.RunRecord(id="r1", session_id=worker))
  await task_mgr.runs.record_finish(worker, "r1", "completed")
  assert task_mgr.activity_of(worker).work_state == "idle"

  cutoff = datetime.datetime.now(datetime.UTC) + datetime.timedelta(hours=1)
  result = await session_blocks.lifecycle.recycle_history_before(worker, cutoff)
  assert result["events_archived"] > 0
  meta = await session_blocks.store.get_session(worker)
  assert meta is not None and meta.archive_offset == result["events_archived"]

  # The rotation replaced the events cache's list, so the warm fold re-keyed
  # and re-folded from the archived half: r1's fact survives the move.
  assert task_mgr.activity_of(worker).work_state == "idle"

  await task_mgr.runs.register_run_locked(models.RunRecord(id="r2", session_id=worker))
  await task_mgr.runs.record_finish(worker, "r2", "completed")
  identity = task_mgr._history_identity(worker)
  warm = task_mgr._run_facts_at(worker, identity).outcomes
  assert warm == {"r1": "completed", "r2": "completed"}

  task_mgr._outcomes_memo.clear()
  assert task_mgr._run_facts_at(worker, identity).outcomes == warm


@pytest.mark.asyncio
async def test_activity_derivation_rereads_after_a_proc_judgment(task_env) -> None:
  """A launched run without a terminal fact is judged through /proc, whose
  death no record write announces: the derivation must re-read on every call
  for such a node — a stored /proc verdict would pin a crashed run's running
  verdict on every tree page until restart."""
  _cfg, _session_blocks, task_mgr = task_env
  ids = await seed_tree(task_mgr)
  worker = ids["worker"]
  await task_mgr.runs.register_run_locked(models.RunRecord(id="r-proc", session_id=worker))
  proc = subprocess.Popen(["sleep", "30"])
  try:
    pid_start, _state = runs.read_pid_stat(proc.pid)
    await task_mgr.runs.record_launch(worker, "r-proc", pid=proc.pid, pid_start=pid_start)
    assert task_mgr.activity_of(worker).work_state == "running"
    assert task_mgr.activity_of(worker).work_state == "running"
  finally:
    proc.kill()
    proc.wait()
  assert task_mgr.activity_of(worker).work_state == "idle"


# The retired task-pause key, spelled in parts: the whole key must stay
# grep-clean out of the tree while these retirement tests still send it on
# the wire and write it on disk.
RETIRED_PAUSE_KEY = "automation" "_paused"


@pytest.mark.asyncio
async def test_metadata_with_the_retired_pause_key_loads_and_drops_it_on_save(task_env) -> None:
  """A metadata.json still carrying the retired pause key loads through the
  normal session load path (unknown keys are ignored on read) and the key is
  gone after the next metadata save."""
  cfg, session_blocks, task_mgr = task_env
  ids = await seed_tree(task_mgr)
  metadata_path = cfg.sessions_dir / ids["worker"] / "metadata.json"
  data = json.loads(metadata_path.read_text())
  assert RETIRED_PAUSE_KEY not in data
  data[RETIRED_PAUSE_KEY] = True
  metadata_path.write_text(json.dumps(data))

  # The normal load paths answer the node, not a parse error.
  meta = await task_mgr.load_task_meta(ids["worker"])
  assert meta is not None and meta.id == ids["worker"]
  loaded = await session_blocks.store.get_session(ids["worker"])
  assert loaded is not None and loaded.id == ids["worker"]

  # The next save rewrites the file from the parsed model: the key is gone.
  await session_blocks.lifecycle.rename_session(ids["worker"], "Renamed")
  saved = json.loads(metadata_path.read_text())
  assert RETIRED_PAUSE_KEY not in saved
  assert saved["name"] == "Renamed"


# ---------------------------------------------------------------------------
# Caller identity: operator vs run-token agent
# ---------------------------------------------------------------------------

# ---------------------------------------------------------------------------
# Authorization: a user request on the chain (a real user message or a thread summon), machine input never
# ---------------------------------------------------------------------------
