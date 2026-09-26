"""The tree index's rebuild concurrency: one build serves every reader of one
invalidation, and a write landing mid-build never installs over its generation.

The sidebar poll, the tree page, and every delegation read the index through
:meth:`TaskTreeManager._get_index`; a structural write invalidates it, and the
readers the write touches all arrive inside the same burst.
"""

from __future__ import annotations

import asyncio
from pathlib import Path

import pytest
import pytest_asyncio
from conftest import build_env

from src.core.run_token import CallerIdentity

OP = CallerIdentity(kind="operator")


@pytest_asyncio.fixture
async def tree(tmp_path: Path):
  _cfg, _session_mgr, tree = build_env(tmp_path)
  await tree.create_task(
      request_id="root", task_parent_id=None, profile="manager", task=None, name="Root", backend=None, caller=OP)
  return tree


async def _count_builds(tree, monkeypatch):
  builds = {"n": 0}
  orig = tree._build_index_sync

  def counting(cached_metas):
    builds["n"] += 1
    return orig(cached_metas)

  monkeypatch.setattr(tree, "_build_index_sync", counting)
  return builds


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
async def test_failed_build_releases_the_marker_and_the_next_read_rebuilds(tree, monkeypatch):
  """One transient build failure must not poison later reads with its exception."""
  builds = await _count_builds(tree, monkeypatch)
  await tree._get_index()
  orig = tree._build_index_sync
  state = {"fail": True}

  def flaky(cached_metas):
    if state["fail"]:
      state["fail"] = False
      raise RuntimeError("transient build failure")
    return orig(cached_metas)

  monkeypatch.setattr(tree, "_build_index_sync", flaky)
  tree._invalidate_index()
  with pytest.raises(RuntimeError):
    await tree._get_index()
  assert tree._index_build_task is None  # the failed build released the marker
  assert (await tree._get_index()).revision  # the next read rebuilt fresh
  assert builds["n"] == 2  # warm-up + the retry; the failed attempt raised before the counter


@pytest.mark.asyncio
async def test_write_mid_build_never_installs_over_newer_generation(tree, monkeypatch):
  """A structural write landing inside a running build refuses that build's install."""
  builds = await _count_builds(tree, monkeypatch)
  await tree._get_index()
  orig = tree._build_index_sync

  def invalidating_build(cached_metas):
    index = orig(cached_metas)
    tree._invalidate_index()  # the write lands while the build runs
    return index

  monkeypatch.setattr(tree, "_build_index_sync", invalidating_build)
  tree._invalidate_index()
  index = await tree._get_index()
  assert index is not None  # the caller still reads its own snapshot
  assert tree._index is None  # never installed over the newer generation
  fresh = await tree._get_index()
  assert builds["n"] == 3  # the post-write read rebuilt
  assert fresh.revision != index.revision or fresh is not index


@pytest.mark.asyncio
async def test_retained_build_on_a_closed_loop_is_rebuilt_not_awaited(tree, monkeypatch):
  """The mixed-loop shape (TestClient's portal vs the test's loop) can leave
    the single-flight marker holding a build still pending on a loop that has
    closed; the reader rebuilds on its own loop instead of awaiting a
    foreign-loop task."""
  builds = await _count_builds(tree, monkeypatch)
  seed_loop = asyncio.new_event_loop()
  try:
    # create_task on a never-running loop parks the task pending; the loop
    # close discards its first step, so it never completes.
    parked = seed_loop.create_task(asyncio.sleep(3600), name="task-tree-index-build")
  finally:
    seed_loop.close()
  tree._invalidate_index()
  tree._index_build_task = parked
  tree._index_build_generation = tree._index_generation
  index = await asyncio.wait_for(tree._get_index(), 5.0)
  assert index.metas
  assert builds["n"] == 1
  assert tree._index_build_task is None
