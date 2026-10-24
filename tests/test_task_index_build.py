"""The tree index's rebuild concurrency: one build serves every reader of one
invalidation, and a write landing mid-build never installs over its generation.
The completion, cancellation and patch checks read the index their caller holds,
so a build no write let install still serves them.

The sidebar poll, the tree page, and every delegation read the index through
:meth:`TaskTreeManager._get_index`; a structural write invalidates it, and the
readers the write touches all arrive inside the same burst.
"""

from __future__ import annotations

import asyncio
import pathlib

import conftest
import pytest
import pytest_asyncio

from src.infra import event_types as ET
from src.infra import models
from src.runtime import control_events, task_completion, task_errors


@pytest_asyncio.fixture
async def tree(tmp_path: pathlib.Path):
  _cfg, _session_blocks, tree = conftest.build_env(tmp_path)
  await conftest.create_task(tree, parent=None, request_id="root", name="Root")
  return tree


async def _count_builds(tree, monkeypatch):
  builds = {"n": 0}
  orig = tree._build_index_sync

  def counting(cached_metas):
    builds["n"] += 1
    return orig(cached_metas)

  monkeypatch.setattr(tree, "_build_index_sync", counting)
  return builds


def _invalidate_during_builds(tree, monkeypatch):
  """Every build from now on is overtaken by a write, so the cache slot stays empty."""
  orig = tree._build_index_sync

  def invalidating_build(cached_metas):
    index = orig(cached_metas)
    tree._invalidate_index()  # the write lands while the build runs
    return index

  monkeypatch.setattr(tree, "_build_index_sync", invalidating_build)
  tree._invalidate_index()


@pytest.mark.asyncio
async def test_concurrent_invalidated_readers_share_one_build(tree, monkeypatch):
  """Every reader one invalidation touches joins one build and one answer."""
  builds = await _count_builds(tree, monkeypatch)
  await tree._get_index()
  builds["n"] = 0  # the warm-up build above is not the burst's
  tree._invalidate_index()
  results = await asyncio.gather(*(tree._get_index() for _ in range(6)))
  assert builds["n"] == 1, f"6 readers of one invalidation spawned {builds['n']} builds"
  # Every reader holds the one object the shared build produced.
  assert all(r is results[0] for r in results)


@pytest.mark.asyncio
async def test_write_mid_build_never_installs_over_newer_generation(tree, monkeypatch):
  """A structural write landing inside a running build refuses that build's install."""
  builds = await _count_builds(tree, monkeypatch)
  await tree._get_index()
  _invalidate_during_builds(tree, monkeypatch)
  index = await tree._get_index()
  assert index is not None  # the caller still reads its own snapshot
  assert tree._index is None  # never installed over the newer generation
  fresh = await tree._get_index()
  assert builds["n"] == 3  # the post-write read rebuilt
  assert fresh.revision != index.revision or fresh is not index


@pytest.mark.asyncio
async def test_index_build_never_folds_a_closed_nodes_full_history(tree, monkeypatch):
  """A closed node's state and archive value come from the history-tail read.

  The structural read and the inheritance fold both need a node's task state;
  the tail read serves both (the second hit is the file-identity memo), so a
  build never loads a closed node's full fold for index output.
  """
  child = await tree.create_task(
      request_id="child",
      task_parent_id=None,
      profile="worker",
      task=None,
      name="Child",
      backend=None,
      caller=conftest.OPERATOR)
  async with tree.control_lock:
    await tree.events.append(
        child.id,
        control_events.build_control_event(
            ET.TASK_CLOSED, actor=control_events.ACTOR_USER, source_session_id=child.id, outcome="completed"))
  calls = {"n": 0}
  orig = type(tree)._facts_of

  def counting(session_id):
    if session_id == child.id:
      calls["n"] += 1
    return orig(tree, session_id)

  monkeypatch.setattr(tree, "_facts_of", counting)
  tree._build_index_sync(tree.session_store.fresh_cached_metas())
  assert calls["n"] == 0, f"closed-node build made {calls['n']} full-fold consults"


@pytest.mark.asyncio
async def test_automatic_completion_closes_when_a_write_lands_mid_build(tree, monkeypatch):
  manager = await conftest.create_task(tree, parent=None, request_id="mgr")
  worker = await conftest.create_task(tree, parent=manager.id, request_id="w", profile="worker")
  await tree.runs.register_run_locked(models.RunRecord(id="run-w", session_id=worker.id, kind="work"))
  _invalidate_during_builds(tree, monkeypatch)
  await tree.dispatch.finish_run(worker.id, "run-w", outcome="success")
  assert tree._index is None  # the builds never installed
  assert tree.task_state(worker.id) == "completed"


@pytest.mark.asyncio
async def test_cancellation_succeeds_when_a_write_lands_mid_build(tree, monkeypatch):
  manager = await conftest.create_task(tree, parent=None, request_id="mgr")
  worker = await conftest.create_task(tree, parent=manager.id, request_id="w", profile="worker")
  _invalidate_during_builds(tree, monkeypatch)
  await tree.completion.cancel_task(worker.id, request_id="cancel", reason="not needed", caller=conftest.OPERATOR)
  assert tree.task_state(worker.id) == "cancelled"


@pytest.mark.asyncio
async def test_manager_archive_revalidates_tree_when_a_write_lands_mid_build(tree, monkeypatch):
  feature = await conftest.create_task(tree, parent=None, request_id="feature")
  worker = await conftest.create_task(tree, parent=feature.id, request_id="w", profile="worker")
  await tree.runs.register_run_locked(models.RunRecord(id="run-w", session_id=worker.id, kind="work"))
  await tree.dispatch.finish_run(worker.id, "run-w", outcome="success")
  # The worker's report is unprocessed input on the manager until a turn consumes it.
  await tree.runs.register_run_locked(models.RunRecord(id="run-turn", session_id=feature.id, kind="manager_turn"))
  async with tree.control_lock:
    await tree.dispatch.claim_input_batch_locked(feature.id, "run-turn")
  await tree.dispatch.finish_run(feature.id, "run-turn", outcome="success")
  _invalidate_during_builds(tree, monkeypatch)
  evidence = task_completion.CompletionEvidence(summary="delivered", result_refs=["run:run-w"], run_ids=["run-w"])
  status, _payload = await tree.completion.archive_task(
      feature.id, request_id="close", evidence=evidence, caller=conftest.OPERATOR)
  assert status == 200
  assert tree.task_state(feature.id) == "archived"


@pytest.mark.asyncio
async def test_structural_patch_runs_its_guards_when_a_write_lands_mid_build(tree, monkeypatch):
  manager = await conftest.create_task(tree, parent=None, request_id="mgr")
  await conftest.create_task(tree, parent=manager.id, request_id="child", profile="worker")
  _invalidate_during_builds(tree, monkeypatch)
  with pytest.raises(task_errors.TaskConflictError, match="no child tasks"):
    await tree.patch_task(manager.id, models.PatchSessionTaskRequest(profile="worker"), caller=conftest.OPERATOR)
