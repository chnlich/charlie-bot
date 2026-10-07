"""Tests for succession-aware trigger firing and master wake protocol."""

from __future__ import annotations

import asyncio
import pathlib
from unittest import mock

import conftest
import pytest

from src.infra import config, models
from src.infra import event_types as ET
from src.runtime import master_trigger, sessions, triggers


@pytest.mark.asyncio
async def test_trigger_master_runs_successor_when_requested_session_eloned(tmp_path: pathlib.Path) -> None:
  cfg = config.CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = sessions.SessionManager(cfg)
  parent_id = await conftest.make_parent(mgr)
  child_id = (await mgr.elone_session(parent_id, event_index=0)).id

  with mock.patch(
      conftest.MASTER_TRIGGER_RUN_MESSAGE_WITH_RESUME_RECOVERY_PATCH_TARGET,
      new=mock.AsyncMock(),
  ) as mock_run:
    await master_trigger.trigger_master(parent_id, "summary", cfg, mgr, ET.CHILD_REPORT)

  mock_run.assert_awaited_once()
  session_meta = mock_run.await_args.args[1]
  assert session_meta.id == child_id


@pytest.mark.asyncio
async def test_firing_trigger_eloned_delivers_into_successor_and_wakes(tmp_path: pathlib.Path) -> None:
  cfg = config.CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = sessions.SessionManager(cfg)
  parent_id = await conftest.make_parent(mgr)
  child_id = (await mgr.elone_session(parent_id, event_index=0)).id
  trigger_mgr = triggers.TriggerManager(cfg, mgr)

  with (
      conftest.patch_trigger_mocks() as mock_master,
      mock.patch(conftest.TRIGGERS_GET_CONFIG_PATCH_TARGET, return_value=cfg),
  ):
    trigger = await trigger_mgr.create_trigger(parent_id, delay_seconds=0, message="wake successor")
    task = trigger_mgr._tasks[trigger.id]
    await asyncio.wait_for(task, timeout=5)

  stored = await trigger_mgr._load_trigger(parent_id, trigger.id)
  assert stored.status == models.TriggerStatus.FIRED

  child_events = mgr.load_chat_events_sync(child_id)
  assert any("wake successor" in ev.get("content", "") for ev in child_events)
  delivered = next(ev for ev in child_events if "wake successor" in ev.get("content", ""))
  assert delivered.get("origin_session_id") == parent_id

  # The trigger record stays in the original session's directory, still FIRED there.
  assert not (mgr._session_dir(child_id) / "triggers" / f"{trigger.id}.json").exists()

  mock_master.assert_awaited_once()
  assert mock_master.await_args.args[0] == parent_id
