"""Tests for the elone succession pointer and successor-chain resolution."""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from conftest import build_session_manager
from conftest import make_parent as _make_parent

from src.infra.config import CharlieBotConfig
from src.infra.models import SessionStatus


@pytest.mark.asyncio
async def test_elone_writes_successor_pointer_and_archives_parent(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = build_session_manager(cfg)
  parent_id = await _make_parent(mgr)

  child = await mgr.elone_session(parent_id, event_index=0)

  fresh_parent = await mgr.store.read_metadata_fresh(parent_id)
  assert fresh_parent is not None
  assert fresh_parent.successor_session_id == child.id
  assert fresh_parent.status == SessionStatus.ARCHIVED
  assert "rating" not in json.loads(mgr.store.metadata_path(parent_id).read_text())


@pytest.mark.asyncio
async def test_resolve_successor_chain_walks_across_three_generations(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = build_session_manager(cfg)
  gen0 = await _make_parent(mgr, name="G0")

  gen1 = await mgr.elone_session(gen0, event_index=0)
  gen2 = await mgr.elone_session(gen1.id, event_index=0)
  gen3 = await mgr.elone_session(gen2.id, event_index=0)

  resolved = await mgr.resolve_successor_chain(gen0)
  assert resolved is not None
  assert resolved.id == gen3.id
