"""Tests for the recap summary path (generate_and_cache_summary)."""

from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from src.core import recap
from src.core.config import CharlieBotConfig
from src.core.models import BackendOption, SessionMetadata
from src.core.recap import generate_and_cache_summary


def _cc_cfg() -> CharlieBotConfig:
  return CharlieBotConfig(
      backend_options=[BackendOption(id="light-cc", label="Light CC", type="cc-claude", model="haiku")],
      model_preference=["light-cc"],
  )


def _mock_backend(one_shot: AsyncMock) -> MagicMock:
  backend = MagicMock()
  backend.one_shot_text = one_shot
  return backend


@pytest.mark.asyncio
async def test_recap_skipped_when_session_missing() -> None:
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = None

  with (
      patch("src.core.recap.build_backend") as mock_build,
      patch("src.core.recap._write_cache_entry") as mock_write,
      patch("src.core.recap.log", new=MagicMock()) as mock_log,
  ):
    result = await generate_and_cache_summary(session_mgr, "missing", 3, _cc_cfg())

  assert result == ""
  mock_build.assert_not_called()
  mock_write.assert_not_called()
  mock_log.warning.assert_called_once_with("recap_skipped", reason="no_session_backend", session_id="missing")


@pytest.mark.asyncio
async def test_recap_returns_empty_on_no_same_type_preference() -> None:
  cfg = CharlieBotConfig(
      backend_options=[
          BackendOption(id="codex-session", label="Session", type="codex", model="gpt-5.5"),
          BackendOption(id="claude-haiku", label="Haiku", type="cc-claude", model="haiku"),
      ],
      model_preference=["claude-haiku"],
  )
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = SessionMetadata(id="s", name="Session 1", backend="codex-session")

  with (
      patch("src.core.recap.build_backend") as mock_build,
      patch("src.core.recap._write_cache_entry") as mock_write,
      patch("src.core.recap.log", new=MagicMock()) as mock_log,
  ):
    result = await generate_and_cache_summary(session_mgr, "s", 3, cfg)

  assert result == ""
  mock_build.assert_not_called()
  mock_write.assert_not_called()
  mock_log.warning.assert_called_once_with(
      "recap_skipped", reason="no_same_type_preference", session_id="s", backend="codex-session")


@pytest.mark.asyncio
async def test_recap_generates_and_caches_on_success() -> None:
  cfg = _cc_cfg()
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = SessionMetadata(id="s", name="Session 1", backend="light-cc")
  one_shot = AsyncMock(return_value="- discussed X\nLast: doing Y")

  with (
      patch("src.core.recap.build_backend", return_value=_mock_backend(one_shot)),
      patch("src.core.recap.extract_recap", return_value={"asks": ["do X"], "last": {"user": "u", "assistant": "a"}}),
      patch("src.core.recap._write_cache_entry") as mock_write,
  ):
    result = await generate_and_cache_summary(session_mgr, "s", 5, cfg)

  assert result == "- discussed X\nLast: doing Y"
  one_shot.assert_awaited_once()
  assert one_shot.await_args.args[1] == recap._SUMMARY_SYSTEM_PROMPT
  mock_write.assert_called_once()
  assert mock_write.call_args.args[3] == "- discussed X\nLast: doing Y"


@pytest.mark.asyncio
async def test_recap_skips_cache_write_when_summary_empty() -> None:
  cfg = _cc_cfg()
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = SessionMetadata(id="s", name="Session 1", backend="light-cc")
  one_shot = AsyncMock(return_value="")

  with (
      patch("src.core.recap.build_backend", return_value=_mock_backend(one_shot)),
      patch("src.core.recap.extract_recap", return_value={"asks": [], "last": None}),
      patch("src.core.recap._write_cache_entry") as mock_write,
  ):
    result = await generate_and_cache_summary(session_mgr, "s", 5, cfg)

  assert result == ""
  mock_write.assert_not_called()
