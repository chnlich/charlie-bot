"""Tests for the elone succession pointer and successor-chain resolution."""

from __future__ import annotations

import json
import pathlib

import conftest
import pytest

from src.infra import config, models


@pytest.mark.asyncio
async def test_elone_writes_successor_pointer_and_archives_parent(tmp_path: pathlib.Path) -> None:
  cfg = config.CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = conftest.build_session_blocks(cfg)
  parent_id = await conftest.make_parent(mgr)

  child = await mgr.fork.elone_session(parent_id, event_index=0)

  fresh_parent = await mgr.store.read_metadata_fresh(parent_id)
  assert fresh_parent is not None
  assert fresh_parent.successor_session_id == child.id
  assert fresh_parent.status == models.SessionStatus.ARCHIVED
  assert "rating" not in json.loads(mgr.store.metadata_path(parent_id).read_text())


@pytest.mark.asyncio
async def test_resolve_successor_chain_walks_across_three_generations(tmp_path: pathlib.Path) -> None:
  cfg = config.CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = conftest.build_session_blocks(cfg)
  gen0 = await conftest.make_parent(mgr, name="G0")

  gen1 = await mgr.fork.elone_session(gen0, event_index=0)
  gen2 = await mgr.fork.elone_session(gen1.id, event_index=0)
  gen3 = await mgr.fork.elone_session(gen2.id, event_index=0)

  resolved = await mgr.successor.resolve_successor_chain(gen0)
  assert resolved is not None
  assert resolved.id == gen3.id
