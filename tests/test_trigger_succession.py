"""Tests for succession-aware trigger firing and master wake protocol."""

from __future__ import annotations

import asyncio
from pathlib import Path
from unittest.mock import AsyncMock, patch

import pytest
from conftest import BROADCAST_PATCH_TARGET, TRIGGER_MASTER_PATCH_TARGET
from conftest import make_parent as _make_parent

from src.core.config import CharlieBotConfig
from src.core.master_trigger import trigger_master
from src.core.models import CreateSessionRequest, TriggerStatus
from src.core.sessions import SessionManager
from src.core.triggers import TriggerManager


@pytest.mark.asyncio
async def test_trigger_master_runs_successor_when_requested_session_eloned(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  parent_id = await _make_parent(mgr)
  child_id = (await mgr.elone_session(parent_id, event_index=0)).id

  with patch(
      "src.core.master_trigger.run_message_with_resume_recovery",
      new=AsyncMock(),
  ) as mock_run:
    await trigger_master(parent_id, "summary", cfg, mgr)

  mock_run.assert_awaited_once()
  session_meta = mock_run.await_args.args[1]
  assert session_meta.id == child_id


@pytest.mark.asyncio
async def test_trigger_master_skips_archived_without_successor(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name="Archived"), backend="claude-opus-4.6")
  await mgr.archive_session(session.id)

  with patch(
      "src.core.master_trigger.run_message_with_resume_recovery",
      new=AsyncMock(),
  ) as mock_run:
    await trigger_master(session.id, "summary", cfg, mgr)

  # No run and no event persisted: the wake is skipped entirely.
  mock_run.assert_not_awaited()
  assert mgr.load_chat_events_sync(session.id) == []


@pytest.mark.asyncio
async def test_firing_trigger_eloned_delivers_into_successor_and_wakes(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  parent_id = await _make_parent(mgr)
  child_id = (await mgr.elone_session(parent_id, event_index=0)).id
  trigger_mgr = TriggerManager(cfg, mgr)

  with (
      patch(BROADCAST_PATCH_TARGET, new=AsyncMock()),
      patch(TRIGGER_MASTER_PATCH_TARGET, new=AsyncMock()) as mock_master,
      patch("src.core.triggers.get_config", return_value=cfg),
  ):
    trigger = await trigger_mgr.create_trigger(parent_id, delay_seconds=0, message="wake successor")
    task = trigger_mgr._tasks[trigger.id]
    await asyncio.wait_for(task, timeout=5)

  stored = await trigger_mgr._load_trigger(parent_id, trigger.id)
  assert stored.status == TriggerStatus.FIRED

  child_events = mgr.load_chat_events_sync(child_id)
  assert any("wake successor" in ev.get("content", "") for ev in child_events)
  delivered = next(ev for ev in child_events if "wake successor" in ev.get("content", ""))
  assert delivered.get("origin_session_id") == parent_id

  # The trigger record stays in the original session's directory, still FIRED there.
  assert not (mgr._session_dir(child_id) / "triggers" / f"{trigger.id}.json").exists()

  mock_master.assert_awaited_once()
  assert mock_master.await_args.args[0] == parent_id


@pytest.mark.asyncio
async def test_firing_trigger_archived_by_hand_without_successor_still_cancels(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name="Archived"), backend="claude-opus-4.6")
  await mgr.archive_session(session.id)
  trigger_mgr = TriggerManager(cfg, mgr)

  with (
      patch(BROADCAST_PATCH_TARGET, new=AsyncMock()),
      patch(TRIGGER_MASTER_PATCH_TARGET, new=AsyncMock()) as mock_master,
  ):
    trigger = await trigger_mgr.create_trigger(session.id, delay_seconds=0, message="hand archived")
    task = trigger_mgr._tasks[trigger.id]
    await asyncio.wait_for(task, timeout=5)

  stored = await trigger_mgr._load_trigger(session.id, trigger.id)
  assert stored.status == TriggerStatus.CANCELLED
  mock_master.assert_not_awaited()
  # Nothing was persisted into the archived session's chat stream.
  assert mgr.load_chat_events_sync(session.id) == []