"""Regression tests for session group change broadcasts."""

from types import SimpleNamespace
from unittest.mock import AsyncMock, patch

import pytest

from src.core import event_types as ET
from src.core.models import SessionMetadata
from src.core.sessions import SessionManager


@pytest.mark.asyncio
async def test_set_group_broadcasts_sidebar_event() -> None:
  session_mgr = SessionManager(SimpleNamespace())
  updated = SessionMetadata(id="session-1", name="Session 1", group="Work")

  with (
      patch.object(session_mgr, "_update_field", new=AsyncMock(return_value=updated)) as mock_update,
      patch("src.core.sessions.streaming_manager.broadcast", new=AsyncMock()) as mock_broadcast,
  ):
    result = await session_mgr.set_group("session-1", "Work")

  assert result == updated
  mock_update.assert_awaited_once_with("session-1", "group", "Work", "session_group_set")
  mock_broadcast.assert_awaited_once_with(
      "sidebar",
      {
          "type": ET.SESSION_GROUP_CHANGED,
          "session_id": "session-1",
          "group": "Work",
      },
  )
