"""Focused regression tests for session autonaming/autogrouping."""

from types import SimpleNamespace
from unittest.mock import AsyncMock, patch

import pytest

from src.core.autonamer import maybe_auto_name
from src.core.models import SessionMetadata


@pytest.mark.asyncio
async def test_maybe_auto_name_passes_existing_groups_to_claude() -> None:
  cfg = SimpleNamespace(gemini_api_key=None, gemini_model="gemini-2.5-flash")
  session_meta = SessionMetadata(id="session-1", name="Session 7")
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = SessionMetadata(id="session-1", name="7: Test Name", group=None)

  with (
      patch(
          "src.core.autonamer._generate_name_via_claude_cli",
          new=AsyncMock(return_value='{"name":"Test Name","group":"work"}'),
      ) as mock_generate,
      patch("src.core.autonamer.streaming_manager.broadcast", new=AsyncMock()),
  ):
    await maybe_auto_name(
        cfg,
        session_meta,
        "Help me review the PR",
        "Here is the review summary.",
        session_mgr,
        ["Work", "Personal"],
    )

  prompt, system_prompt = mock_generate.await_args.args
  assert "Prefer reusing one of these existing groups: [Work, Personal]" in prompt
  assert "Prefer reusing one of these existing groups: [Work, Personal]" in system_prompt
  session_mgr.set_group.assert_awaited_once_with("session-1", "Work")


@pytest.mark.asyncio
async def test_maybe_auto_name_does_not_overwrite_existing_manual_group() -> None:
  cfg = SimpleNamespace(gemini_api_key=None, gemini_model="gemini-2.5-flash")
  session_meta = SessionMetadata(id="session-2", name="Session 8")
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = SessionMetadata(id="session-2", name="8: Test Name", group="Manual")

  with (
      patch(
          "src.core.autonamer._generate_name_via_claude_cli",
          new=AsyncMock(return_value='{"name":"Test Name","group":"Work"}'),
      ),
      patch("src.core.autonamer.streaming_manager.broadcast", new=AsyncMock()) as mock_broadcast,
  ):
    await maybe_auto_name(
        cfg,
        session_meta,
        "Help me review the PR",
        "Here is the review summary.",
        session_mgr,
        ["Work"],
    )

  session_mgr.rename_session.assert_awaited_once_with("session-2", "8: Test Name")
  session_mgr.set_group.assert_not_awaited()
  assert [call.args[0] for call in mock_broadcast.await_args_list] == ["session:session-2", "sidebar"]
