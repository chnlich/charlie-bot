from __future__ import annotations

from datetime import datetime, timezone
from pathlib import Path
from unittest.mock import AsyncMock, patch

import pytest

from src.api.message_utils import events_to_messages
from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.models import CreateSessionRequest, PendingTrigger, TriggerStatus
from src.core.sessions import SessionManager
from src.core.triggers import TriggerManager

VOICE_KEY = "is_" + "voice"


@pytest.mark.asyncio
async def test_delayed_trigger_persists_user_event_and_wakes_master(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "charliebot-home")
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="Delayed trigger"))
  trigger_mgr = TriggerManager(cfg, session_mgr)
  trigger = PendingTrigger(
      id="trigger-1",
      session_id=session.id,
      fire_at=datetime.now(timezone.utc),
      message="Check PID 12345",
  )
  await trigger_mgr._save_trigger(trigger)

  with (
      patch("src.core.sessions.streaming_manager.broadcast", new=AsyncMock()) as mock_broadcast,
      patch("src.core.triggers.trigger_master", new=AsyncMock()) as mock_trigger_master,
      # The wake path re-reads the config instead of using the snapshot captured at
      # construction, so the fresh read is what must reach trigger_master.
      patch("src.core.triggers.get_config", return_value=cfg),
  ):
    await trigger_mgr._wait_and_fire(trigger)

  events = session_mgr.load_chat_events_sync(session.id)
  assert len(events) == 1
  assert events[0]["type"] == ET.SCHEDULED_TRIGGER
  assert events[0]["content"] == "[Scheduled trigger fired] Check PID 12345"
  assert VOICE_KEY not in events[0]

  expected_message = {
      "role": "scheduled_trigger",
      "content": "[Scheduled trigger fired] Check PID 12345",
      "event_index": 0,
      "id": events[0]["id"],
      "timestamp": events[0]["timestamp"],
  }
  messages = events_to_messages(events)
  assert messages == [expected_message]

  channel, broadcast_event = mock_broadcast.await_args.args
  assert channel == f"session:{session.id}"
  # Raw user events are no longer broadcast; the per-session aggregator emits a
  # `message` delta carrying the same payload, which is what the client renders.
  assert broadcast_event == {"type": "message", "message": expected_message}

  mock_trigger_master.assert_awaited_once_with(
      session.id,
      "[Scheduled trigger fired] Check PID 12345",
      cfg,
      session_mgr,
  )

  stored_trigger = await trigger_mgr._load_trigger(session.id, trigger.id)
  assert stored_trigger.status == TriggerStatus.FIRED
  assert stored_trigger.fired_at is not None
  # Pure-delay triggers (no watch targets) converge on the 'timeout' reason.
  assert stored_trigger.fire_reason == "timeout"


@pytest.mark.asyncio
async def test_archived_session_trigger_is_cancelled_without_waking_master(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "charliebot-home")
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="Archived trigger"))
  trigger_mgr = TriggerManager(cfg, session_mgr)
  trigger = PendingTrigger(
      id="archived-trigger",
      session_id=session.id,
      fire_at=datetime.now(timezone.utc),
      message="must not wake",
  )
  await trigger_mgr._save_trigger(trigger)
  await session_mgr.archive_session(session.id)

  with (
      patch("src.core.sessions.streaming_manager.broadcast", new=AsyncMock()) as mock_broadcast,
      patch("src.core.triggers.trigger_master", new=AsyncMock()) as mock_trigger_master,
      patch("src.core.triggers.get_config", return_value=cfg),
  ):
    await trigger_mgr._wait_and_fire(trigger)

  assert session_mgr.load_chat_events_sync(session.id) == []
  mock_broadcast.assert_not_awaited()
  mock_trigger_master.assert_not_awaited()
  stored_trigger = await trigger_mgr._load_trigger(session.id, trigger.id)
  assert stored_trigger.status == TriggerStatus.CANCELLED


@pytest.mark.asyncio
async def test_trigger_with_missing_session_metadata_is_cancelled_without_waking_master(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "charliebot-home")
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="Missing metadata trigger"))
  trigger_mgr = TriggerManager(cfg, session_mgr)
  trigger = PendingTrigger(
      id="missing-metadata-trigger",
      session_id=session.id,
      fire_at=datetime.now(timezone.utc),
      message="must not wake",
  )
  await trigger_mgr._save_trigger(trigger)
  (cfg.sessions_dir / session.id / "metadata.json").unlink()
  session_mgr._metadata_cache.pop(session.id)

  with (
      patch("src.core.sessions.streaming_manager.broadcast", new=AsyncMock()) as mock_broadcast,
      patch("src.core.triggers.trigger_master", new=AsyncMock()) as mock_trigger_master,
      patch("src.core.triggers.get_config", return_value=cfg),
  ):
    await trigger_mgr._wait_and_fire(trigger)

  assert session_mgr.load_chat_events_sync(session.id) == []
  mock_broadcast.assert_not_awaited()
  mock_trigger_master.assert_not_awaited()
  stored_trigger = await trigger_mgr._load_trigger(session.id, trigger.id)
  assert stored_trigger.status == TriggerStatus.CANCELLED


@pytest.mark.asyncio
async def test_trigger_with_empty_session_metadata_is_cancelled_without_waking_master(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "charliebot-home")
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="Empty metadata trigger"))
  trigger_mgr = TriggerManager(cfg, session_mgr)
  trigger = PendingTrigger(
      id="empty-metadata-trigger",
      session_id=session.id,
      fire_at=datetime.now(timezone.utc),
      message="must not wake",
  )
  await trigger_mgr._save_trigger(trigger)
  (cfg.sessions_dir / session.id / "metadata.json").write_text("")
  session_mgr._metadata_cache.pop(session.id)

  with (
      patch("src.core.sessions.streaming_manager.broadcast", new=AsyncMock()) as mock_broadcast,
      patch("src.core.triggers.trigger_master", new=AsyncMock()) as mock_trigger_master,
      patch("src.core.triggers.get_config", return_value=cfg),
  ):
    await trigger_mgr._wait_and_fire(trigger)

  assert session_mgr.load_chat_events_sync(session.id) == []
  mock_broadcast.assert_not_awaited()
  mock_trigger_master.assert_not_awaited()
  stored_trigger = await trigger_mgr._load_trigger(session.id, trigger.id)
  assert stored_trigger.status == TriggerStatus.CANCELLED
