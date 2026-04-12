from __future__ import annotations

from datetime import datetime, timezone
from pathlib import Path
from unittest.mock import AsyncMock, patch

import pytest

from src.api.message_utils import events_to_messages
from src.core.config import CharlieBotConfig
from src.core.models import CreateSessionRequest, PendingTrigger, TriggerStatus
from src.core.sessions import SessionManager
from src.core.triggers import TriggerManager


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
  ):
    await trigger_mgr._wait_and_fire(trigger)

  events = session_mgr.load_chat_events_sync(session.id)
  assert len(events) == 1
  assert events[0]["type"] == "user"
  assert events[0]["content"] == "[Scheduled trigger fired] Check PID 12345"
  assert events[0]["is_voice"] is False

  messages = events_to_messages(events)
  assert messages == [
      {
          "role": "user",
          "content": "[Scheduled trigger fired] Check PID 12345",
          "uploaded_files": [],
          "is_voice": False,
          "event_index": 0,
          "timestamp": events[0]["timestamp"],
      },
  ]

  channel, broadcast_event = mock_broadcast.await_args.args
  assert channel == f"session:{session.id}"
  assert broadcast_event == events[0]

  mock_trigger_master.assert_awaited_once_with(
      session.id,
      "[Scheduled trigger fired] Check PID 12345",
      cfg,
      session_mgr,
  )

  stored_trigger = await trigger_mgr._load_trigger(session.id, trigger.id)
  assert stored_trigger.status == TriggerStatus.FIRED
  assert stored_trigger.fired_at is not None
