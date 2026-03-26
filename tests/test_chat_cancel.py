"""Regression tests for master cancel endpoint behavior."""

from unittest.mock import AsyncMock, patch

import pytest
from fastapi import HTTPException

from src.api.chat import cancel_master_agent


@pytest.mark.asyncio
async def test_cancel_master_agent_success() -> None:
  session_mgr = AsyncMock()

  with patch("src.api.chat.cancel_master", new=AsyncMock(return_value=True)) as mock_cancel:
    result = await cancel_master_agent("session-ok", _meta=object(), session_mgr=session_mgr)

  assert result == {"ok": True}
  mock_cancel.assert_awaited_once_with("session-ok")
  session_mgr.persist_and_broadcast.assert_not_awaited()


@pytest.mark.asyncio
async def test_cancel_master_agent_no_active_master_broadcasts_error() -> None:
  session_mgr = AsyncMock()

  with patch("src.api.chat.cancel_master", new=AsyncMock(return_value=False)) as mock_cancel:
    with pytest.raises(HTTPException) as exc_info:
      await cancel_master_agent("session-missing", _meta=object(), session_mgr=session_mgr)

  assert exc_info.value.status_code == 404
  assert exc_info.value.detail == "No active master agent"
  mock_cancel.assert_awaited_once_with("session-missing")
  session_mgr.persist_and_broadcast.assert_awaited_once_with(
      "session-missing",
      {
          "type": "assistant_error",
          "content": "No active master agent to cancel.",
      },
  )
