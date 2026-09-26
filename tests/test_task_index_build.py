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
    cfg, session_mgr, tree = build_env(tmp_path)
    await tree.create_task(
        request_id="root", task_parent_id=None, profile="manager", task=None, name="Root",
        backend=None, caller=OP)
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
