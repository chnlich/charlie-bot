"""Tests for succession-aware trigger firing and master wake protocol."""

from __future__ import annotations

import asyncio
import os
from pathlib import Path
from unittest.mock import AsyncMock, patch

import pytest
from conftest import (
    BROADCAST_PATCH_TARGET,
    OPUS_BACKEND_ID,
    TRIGGER_MASTER_PATCH_TARGET,
    TRIGGERS_GET_CONFIG_PATCH_TARGET,
    make_home_session,
)
from conftest import make_parent as _make_parent
from fastapi import HTTPException

from src.api import internal
from src.core.config import CharlieBotConfig
from src.core.master_trigger import trigger_master
from src.core.models import (
    LocalPid,
    ScheduleTriggerRequest,
    SessionStatus,
    TriggerStatus,
)
from src.core.sessions import SessionManager
from src.core.triggers import ArchivedSessionError, TriggerManager


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
async def test_trigger_master_opted_out_skips_archived_without_successor(tmp_path: Path) -> None:
  """pull_back=False is the timed-wake opt-out shared by the trigger fire and the cron
  scheduler: the skip keeps the session archived with no run and no event."""
  cfg, mgr, session = await make_home_session(tmp_path, name="Archived", backend=OPUS_BACKEND_ID)
  await mgr.archive_session(session.id)

  with patch(
      "src.core.master_trigger.run_message_with_resume_recovery",
      new=AsyncMock(),
  ) as mock_run:
    await trigger_master(session.id, "summary", cfg, mgr, pull_back=False)

  mock_run.assert_not_awaited()
  assert mgr.load_chat_events_sync(session.id) == []
  fresh = await mgr.get_session(session.id)
  assert fresh is not None
  assert fresh.status == SessionStatus.ARCHIVED


@pytest.mark.asyncio
async def test_trigger_master_default_pulls_archived_session_back_to_active(tmp_path: Path) -> None:
  """A caller that passes no pull_back carries content: the archived session is
  unarchived and the wake proceeds against it."""
  cfg, mgr, session = await make_home_session(tmp_path, name="Archived", backend=OPUS_BACKEND_ID)
  await mgr.archive_session(session.id)

  with patch(
      "src.core.master_trigger.run_message_with_resume_recovery",
      new=AsyncMock(),
  ) as mock_run:
    await trigger_master(session.id, "summary", cfg, mgr)

  mock_run.assert_awaited_once()
  assert mock_run.await_args.args[1].id == session.id
  fresh = await mgr.get_session(session.id)
  assert fresh is not None
  assert fresh.status == SessionStatus.ACTIVE


@pytest.mark.asyncio
async def test_create_trigger_rejects_archived_session_without_successor(tmp_path: Path) -> None:
  cfg, mgr, session = await make_home_session(tmp_path, name="Archived", backend=OPUS_BACKEND_ID)
  await mgr.archive_session(session.id)
  trigger_mgr = TriggerManager(cfg, mgr)

  with pytest.raises(ArchivedSessionError, match=session.id):
    await trigger_mgr.create_trigger(session.id, delay_seconds=60, message="too late")


@pytest.mark.asyncio
async def test_schedule_trigger_api_maps_archived_session_to_422(tmp_path: Path) -> None:
  """The internal API surfaces the create-time rejection as 422 naming the session,
  so the CLI's existing 422 -> exit-2 mapping reports it."""
  cfg, mgr, session = await make_home_session(tmp_path, name="Archived", backend=OPUS_BACKEND_ID)
  await mgr.archive_session(session.id)

  with pytest.raises(HTTPException) as exc_info:
    await internal.schedule_trigger(
        ScheduleTriggerRequest(session_id=session.id, delay_seconds=60, message="wake"),
        session_mgr=mgr,
        trigger_mgr=TriggerManager(cfg, mgr),
    )
  assert exc_info.value.status_code == 422
  assert session.id in exc_info.value.detail


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
      patch(TRIGGERS_GET_CONFIG_PATCH_TARGET, return_value=cfg),
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
async def test_watch_trigger_cancelled_when_session_archived_mid_wait(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    pidfd_open_available: None,
) -> None:
  """The watchdog, not the wait, wins when a watched session is archived mid-wait:
  within one check interval the record reads CANCELLED and the waiter task is done."""
  cfg, mgr, session = await make_home_session(tmp_path, name="Watched", backend=OPUS_BACKEND_ID)
  trigger_mgr = TriggerManager(cfg, mgr)
  # Drive the clock: poll the dormancy predicate every 50ms instead of 60s.
  monkeypatch.setattr("src.core.triggers._DORMANCY_CHECK_SECONDS", 0.05)

  with (
      patch(BROADCAST_PATCH_TARGET, new=AsyncMock()),
      patch(TRIGGER_MASTER_PATCH_TARGET, new=AsyncMock()) as mock_master,
  ):
    # A pid that stays alive for the whole test: the wait would otherwise run to
    # its 3600s deadline, so only the watchdog can end this trigger.
    trigger = await trigger_mgr.create_trigger(
        session.id,
        delay_seconds=3600,
        message="watch a live pid",
        watch_targets=[LocalPid(pid=os.getpid())],
    )
    task = trigger_mgr._tasks[trigger.id]
    await mgr.archive_session(session.id)
    await asyncio.wait_for(task, timeout=5)

  stored = await trigger_mgr._load_trigger(session.id, trigger.id)
  assert stored.status == TriggerStatus.CANCELLED
  mock_master.assert_not_awaited()
  assert mgr.load_chat_events_sync(session.id) == []


@pytest.mark.asyncio
async def test_pure_delay_trigger_cancelled_when_session_archived_mid_wait(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  """The plain-sleep wait races the same watchdog: an archive mid-wait cancels
  before the (hour-away) deadline."""
  cfg, mgr, session = await make_home_session(tmp_path, name="Delayed", backend=OPUS_BACKEND_ID)
  trigger_mgr = TriggerManager(cfg, mgr)
  monkeypatch.setattr("src.core.triggers._DORMANCY_CHECK_SECONDS", 0.05)

  with (
      patch(BROADCAST_PATCH_TARGET, new=AsyncMock()),
      patch(TRIGGER_MASTER_PATCH_TARGET, new=AsyncMock()) as mock_master,
  ):
    trigger = await trigger_mgr.create_trigger(session.id, delay_seconds=3600, message="pure delay")
    task = trigger_mgr._tasks[trigger.id]
    await mgr.archive_session(session.id)
    await asyncio.wait_for(task, timeout=5)

  stored = await trigger_mgr._load_trigger(session.id, trigger.id)
  assert stored.status == TriggerStatus.CANCELLED
  mock_master.assert_not_awaited()


@pytest.mark.asyncio
async def test_fire_time_backstop_cancels_when_archive_lands_before_fire(tmp_path: Path) -> None:
  """With the watchdog still far from its next poll (default 60s interval), the
  fire-time re-check of the same predicate catches an archive that landed during
  a short wait."""
  cfg, mgr, session = await make_home_session(tmp_path, name="Late archive", backend=OPUS_BACKEND_ID)
  trigger_mgr = TriggerManager(cfg, mgr)

  with (
      patch(BROADCAST_PATCH_TARGET, new=AsyncMock()),
      patch(TRIGGER_MASTER_PATCH_TARGET, new=AsyncMock()) as mock_master,
  ):
    trigger = await trigger_mgr.create_trigger(session.id, delay_seconds=1, message="backstop")
    task = trigger_mgr._tasks[trigger.id]
    await mgr.archive_session(session.id)
    await asyncio.wait_for(task, timeout=5)

  stored = await trigger_mgr._load_trigger(session.id, trigger.id)
  assert stored.status == TriggerStatus.CANCELLED
  mock_master.assert_not_awaited()
  # The opted-out wake never ran, so the session stays archived with no event.
  fresh = await mgr.get_session(session.id)
  assert fresh is not None
  assert fresh.status == SessionStatus.ARCHIVED
  assert mgr.load_chat_events_sync(session.id) == []


@pytest.mark.asyncio
async def test_trigger_archived_mid_wait_with_successor_still_fires_into_successor(tmp_path: Path,) -> None:
  """An archive with a successor is succession, not dormancy: the trigger must NOT
  be cancelled and still delivers into the chain end."""
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  parent_id = await _make_parent(mgr)
  trigger_mgr = TriggerManager(cfg, mgr)

  with (
      patch(BROADCAST_PATCH_TARGET, new=AsyncMock()),
      patch(TRIGGER_MASTER_PATCH_TARGET, new=AsyncMock()) as mock_master,
      patch(TRIGGERS_GET_CONFIG_PATCH_TARGET, return_value=cfg),
  ):
    trigger = await trigger_mgr.create_trigger(parent_id, delay_seconds=1, message="wake the child")
    task = trigger_mgr._tasks[trigger.id]
    child_id = (await mgr.elone_session(parent_id, event_index=0)).id
    await asyncio.wait_for(task, timeout=5)

  stored = await trigger_mgr._load_trigger(parent_id, trigger.id)
  assert stored.status == TriggerStatus.FIRED
  child_events = mgr.load_chat_events_sync(child_id)
  assert any("wake the child" in ev.get("content", "") for ev in child_events)
  mock_master.assert_awaited_once()
  assert mock_master.await_args.args[0] == parent_id
