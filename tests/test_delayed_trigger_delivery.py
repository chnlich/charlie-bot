from __future__ import annotations

import asyncio
import enum
from datetime import UTC, datetime
from pathlib import Path
from unittest.mock import AsyncMock, patch

import pytest
from conftest import (
    BROADCAST_PATCH_TARGET,
    TRIGGER_MASTER_PATCH_TARGET,
    TRIGGERS_GET_CONFIG_PATCH_TARGET,
    make_home_config,
)

from src.api.message_utils import events_to_messages
from src.core import event_types as ET
from src.core.models import CreateSessionRequest, PendingTrigger, TriggerStatus
from src.core.sessions import SessionManager
from src.core.triggers import TriggerManager

VOICE_KEY = "is_voice"


@pytest.mark.asyncio
async def test_delayed_trigger_persists_user_event_and_wakes_master(tmp_path: Path) -> None:
  cfg = make_home_config(tmp_path)
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="Delayed trigger"))
  trigger_mgr = TriggerManager(cfg, session_mgr)
  trigger = PendingTrigger(
      id="trigger-1",
      session_id=session.id,
      fire_at=datetime.now(UTC),
      message="Check PID 12345",
  )
  await trigger_mgr._save_trigger(trigger)

  with (
      patch(BROADCAST_PATCH_TARGET, new=AsyncMock()) as mock_broadcast,
      patch(TRIGGER_MASTER_PATCH_TARGET, new=AsyncMock()) as mock_trigger_master,
      # The wake path re-reads the config instead of using the snapshot captured at
      # construction, so the fresh read is what must reach trigger_master.
      patch(TRIGGERS_GET_CONFIG_PATCH_TARGET, return_value=cfg),
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
  # Raw user events are not broadcast; the per-session aggregator emits a
  # `message` delta carrying the same payload, which is what the client renders.
  assert broadcast_event == {"type": "message", "message": expected_message}

  # FIRED on delivery: the record leaves pending before the wake is enqueued,
  # and the wake's task no longer waits for the woken turn.
  mock_trigger_master.assert_called_once_with(
      session.id,
      "[Scheduled trigger fired] Check PID 12345",
      cfg,
      session_mgr,
      ET.SCHEDULED_TRIGGER,
      user_event_id=events[0]["id"],
      # Timed wake: the fire passes the opted-out pull_back.
      pull_back=False,
  )
  assert mock_trigger_master.call_args.kwargs["user_event_id"] == events[0]["id"]

  stored_trigger = await trigger_mgr._load_trigger(session.id, trigger.id)
  assert stored_trigger.status == TriggerStatus.FIRED
  assert stored_trigger.fired_at is not None
  # Pure-delay triggers (no watch targets) converge on the 'timeout' reason.
  assert stored_trigger.fire_reason == "timeout"


class _Invalidation(enum.Enum):
  ARCHIVED = 1
  MISSING_METADATA = 2
  EMPTY_METADATA = 3


@pytest.mark.asyncio
@pytest.mark.parametrize("invalidation", list(_Invalidation))
async def test_invalid_session_trigger_is_cancelled_without_waking_master(
    tmp_path: Path, invalidation: _Invalidation) -> None:
  cfg = make_home_config(tmp_path)
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="Invalid session trigger"))
  trigger_mgr = TriggerManager(cfg, session_mgr)
  trigger = PendingTrigger(
      id="invalid-session-trigger",
      session_id=session.id,
      fire_at=datetime.now(UTC),
      message="must not wake",
  )
  await trigger_mgr._save_trigger(trigger)
  metadata_path = cfg.sessions_dir / session.id / "metadata.json"
  if invalidation is _Invalidation.ARCHIVED:
    await session_mgr.archive_session(session.id)
  elif invalidation is _Invalidation.MISSING_METADATA:
    metadata_path.unlink()
    session_mgr._metadata_cache.pop(session.id)
  elif invalidation is _Invalidation.EMPTY_METADATA:
    metadata_path.write_text("")
    session_mgr._metadata_cache.pop(session.id)
  else:
    raise AssertionError(f"unhandled invalidation: {invalidation}")

  with (
      patch(BROADCAST_PATCH_TARGET, new=AsyncMock()) as mock_broadcast,
      patch(TRIGGER_MASTER_PATCH_TARGET, new=AsyncMock()) as mock_trigger_master,
      patch(TRIGGERS_GET_CONFIG_PATCH_TARGET, return_value=cfg),
  ):
    await trigger_mgr._wait_and_fire(trigger)

  assert not session_mgr.load_chat_events_sync(session.id)
  mock_broadcast.assert_not_awaited()
  mock_trigger_master.assert_not_awaited()
  stored_trigger = await trigger_mgr._load_trigger(session.id, trigger.id)
  assert stored_trigger.status == TriggerStatus.CANCELLED


@pytest.mark.asyncio
async def test_trigger_says_fired_the_moment_its_wake_is_enqueued(tmp_path: Path) -> None:
  """FIRED on delivery: at the instant the wake's work item is enqueued, the
  record on disk already says fired and the pending-triggers tray no longer
  lists it."""
  from fastapi import FastAPI
  from fastapi.testclient import TestClient

  from src.api.deps import get_session_manager, get_trigger_manager
  from src.api.sessions import router as sessions_router

  cfg = make_home_config(tmp_path)
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="Fired on delivery"))
  trigger_mgr = TriggerManager(cfg, session_mgr)
  trigger = PendingTrigger(
      id="trigger-fired-on-delivery",
      session_id=session.id,
      fire_at=datetime.now(UTC),
      message="wake now",
  )
  await trigger_mgr._save_trigger(trigger)

  observed: dict = {}

  async def fake_trigger_master(sid, message, got_cfg, got_mgr, etype, *, user_event_id=None, pull_back=True):
    # This runs at the enqueue moment, after the FIRED stamp.
    stored = await trigger_mgr._load_trigger(sid, "trigger-fired-on-delivery")
    observed["status"] = stored.status
    observed["input_event_id"] = user_event_id
    observed["tray"] = await trigger_mgr.list_triggers(sid)

  with (
      patch(BROADCAST_PATCH_TARGET, new=AsyncMock()),
      patch(TRIGGER_MASTER_PATCH_TARGET, new=fake_trigger_master),
      patch(TRIGGERS_GET_CONFIG_PATCH_TARGET, return_value=cfg),
  ):
    await trigger_mgr._wait_and_fire(trigger)
    # The wake enqueue is a fire-and-forget task; wait it out to its checkpoint.
    async with asyncio.timeout(5):
      while "status" not in observed:
        await asyncio.sleep(0.01)

  assert observed["status"] == TriggerStatus.FIRED
  # The wake's event id is what the enqueued turn answers.
  wake_events = [e for e in session_mgr.load_chat_events_sync(session.id) if e["type"] == ET.SCHEDULED_TRIGGER]
  assert observed["input_event_id"] == wake_events[0]["id"]
  # The tray endpoint lists pending only: the just-delivered record is out.
  app = FastAPI()
  app.include_router(sessions_router, prefix="/api/sessions")
  app.dependency_overrides[get_session_manager] = lambda: session_mgr
  app.dependency_overrides[get_trigger_manager] = lambda: trigger_mgr
  tray = TestClient(app).get(f"/api/sessions/{session.id}/pending-triggers")
  assert tray.status_code == 200
  assert [row["id"] for row in tray.json()] == []
