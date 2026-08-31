"""Tests for the recap summary path (generate_and_cache_summary)."""

from unittest.mock import AsyncMock, MagicMock, patch

import pytest
from conftest import make_one_shot_backend

from src.core import recap
from src.core.config import CharlieBotConfig
from src.core.models import BackendOption, SessionMetadata
from src.core.recap import generate_and_cache_summary

# Import-path patch targets for the recap seams. src/core/recap.py binds build_backend at
# import scope (`from src.agents.backends.registry import build_backend`) and defines
# extract_recap, _write_cache_entry, and log at module scope, so patch() lands the stand-in
# on the src.core.recap module attribute and generate_and_cache_summary reads it at call
# time; a drifted string copy would patch a name nothing reads.
_BUILD_BACKEND_PATCH_TARGET = "src.core.recap.build_backend"
_EXTRACT_RECAP_PATCH_TARGET = "src.core.recap.extract_recap"
_WRITE_CACHE_ENTRY_PATCH_TARGET = "src.core.recap._write_cache_entry"
_LOG_PATCH_TARGET = "src.core.recap.log"


def _cc_cfg() -> CharlieBotConfig:
  return CharlieBotConfig(
      backend_options=[BackendOption(id="light-cc", label="Light CC", type="cc-claude", model="haiku")],
      model_preference=["light-cc"],
  )


@pytest.mark.asyncio
async def test_recap_skipped_when_session_missing() -> None:
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = None

  with (
      patch(_BUILD_BACKEND_PATCH_TARGET) as mock_build,
      patch(_WRITE_CACHE_ENTRY_PATCH_TARGET) as mock_write,
      patch(_LOG_PATCH_TARGET, new=MagicMock()) as mock_log,
  ):
    result = await generate_and_cache_summary(session_mgr, "missing", 3, _cc_cfg())

  assert result == ""
  mock_build.assert_not_called()
  mock_write.assert_not_called()
  mock_log.warning.assert_called_once_with("recap_skipped", reason="no_session_backend", session_id="missing")


@pytest.mark.asyncio
async def test_recap_returns_empty_on_no_resolvable_preference() -> None:
  cfg = CharlieBotConfig(
      backend_options=[
          BackendOption(id="claude-haiku", label="Haiku", type="cc-claude", model="haiku"),
      ],
      model_preference=["does-not-exist"],
  )
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = SessionMetadata(id="s", name="Session 1", backend="codex-session")

  with (
      patch(_BUILD_BACKEND_PATCH_TARGET) as mock_build,
      patch(_WRITE_CACHE_ENTRY_PATCH_TARGET) as mock_write,
      patch(_LOG_PATCH_TARGET, new=MagicMock()) as mock_log,
  ):
    result = await generate_and_cache_summary(session_mgr, "s", 3, cfg)

  assert result == ""
  mock_build.assert_not_called()
  mock_write.assert_not_called()
  mock_log.warning.assert_called_once_with("recap_skipped", reason="no_resolvable_preference", session_id="s")


@pytest.mark.asyncio
async def test_recap_generates_and_caches_on_success() -> None:
  cfg = _cc_cfg()
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = SessionMetadata(id="s", name="Session 1", backend="light-cc")
  one_shot = AsyncMock(return_value="- discussed X\nLast: doing Y")

  with (
      patch(_BUILD_BACKEND_PATCH_TARGET, return_value=make_one_shot_backend(one_shot)),
      patch(_EXTRACT_RECAP_PATCH_TARGET, return_value={"asks": ["do X"], "last": {"user": "u", "assistant": "a"}}),
      patch(_WRITE_CACHE_ENTRY_PATCH_TARGET) as mock_write,
  ):
    result = await generate_and_cache_summary(session_mgr, "s", 5, cfg)

  assert result == "- discussed X\nLast: doing Y"
  one_shot.assert_awaited_once()
  assert one_shot.await_args.args[1] == recap._SUMMARY_SYSTEM_PROMPT
  mock_write.assert_called_once()
  assert mock_write.call_args.args[3] == "- discussed X\nLast: doing Y"


@pytest.mark.asyncio
async def test_recap_uses_preferences_when_session_backend_is_empty() -> None:
  cfg = _cc_cfg()
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = SessionMetadata(id="s", name="Session 1", backend="")
  one_shot = AsyncMock(return_value="summary")

  with (
      patch(_BUILD_BACKEND_PATCH_TARGET, return_value=make_one_shot_backend(one_shot)),
      patch(_EXTRACT_RECAP_PATCH_TARGET, return_value={"asks": [], "last": None}),
      patch(_WRITE_CACHE_ENTRY_PATCH_TARGET) as mock_write,
  ):
    result = await generate_and_cache_summary(session_mgr, "s", 5, cfg)

  assert result == "summary"
  one_shot.assert_awaited_once()
  mock_write.assert_called_once()


@pytest.mark.asyncio
async def test_recap_falls_back_after_first_candidate_failure() -> None:
  cfg = CharlieBotConfig(
      backend_options=[
          BackendOption(id="first", label="First", type="cc-claude", model="haiku"),
          BackendOption(id="second", label="Second", type="codex", model="gpt-x"),
      ],
      model_preference=["first", "second"],
  )
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = SessionMetadata(id="s", name="Session 1", backend="")
  first_one_shot = AsyncMock(side_effect=RuntimeError("first candidate failed"))
  second_one_shot = AsyncMock(return_value="fallback summary")

  with (
      patch(
          _BUILD_BACKEND_PATCH_TARGET,
          side_effect=[make_one_shot_backend(first_one_shot), make_one_shot_backend(second_one_shot)],
      ) as mock_build,
      patch(_EXTRACT_RECAP_PATCH_TARGET, return_value={"asks": ["do X"], "last": None}),
      patch(_WRITE_CACHE_ENTRY_PATCH_TARGET) as mock_write,
  ):
    result = await generate_and_cache_summary(session_mgr, "s", 5, cfg)

  assert result == "fallback summary"
  first_one_shot.assert_awaited_once()
  second_one_shot.assert_awaited_once()
  assert [entry.args[0].id for entry in mock_build.call_args_list] == ["first", "second"]
  mock_write.assert_called_once()


@pytest.mark.asyncio
async def test_recap_returns_empty_without_cache_when_all_candidates_are_empty() -> None:
  cfg = CharlieBotConfig(
      backend_options=[
          BackendOption(id="first", label="First", type="cc-claude", model="haiku"),
          BackendOption(id="second", label="Second", type="codex", model="gpt-x"),
      ],
      model_preference=["first", "second"],
  )
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = SessionMetadata(id="s", name="Session 1", backend="")
  first_one_shot = AsyncMock(return_value="")
  second_one_shot = AsyncMock(return_value="")

  with (
      patch(
          _BUILD_BACKEND_PATCH_TARGET,
          side_effect=[make_one_shot_backend(first_one_shot), make_one_shot_backend(second_one_shot)],
      ) as mock_build,
      patch(_EXTRACT_RECAP_PATCH_TARGET, return_value={"asks": [], "last": None}),
      patch(_WRITE_CACHE_ENTRY_PATCH_TARGET) as mock_write,
  ):
    result = await generate_and_cache_summary(session_mgr, "s", 5, cfg)

  assert result == ""
  first_one_shot.assert_awaited_once()
  second_one_shot.assert_awaited_once()
  assert [entry.args[0].id for entry in mock_build.call_args_list] == ["first", "second"]
  mock_write.assert_not_called()


@pytest.mark.asyncio
async def test_recap_raises_last_exception_when_all_candidates_raise() -> None:
  cfg = CharlieBotConfig(
      backend_options=[
          BackendOption(id="first", label="First", type="cc-claude", model="haiku"),
          BackendOption(id="last", label="Last", type="codex", model="gpt-x"),
      ],
      model_preference=["first", "last"],
  )
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = SessionMetadata(id="s", name="Session 1", backend="")
  first_error = RuntimeError("first error")
  last_error = RuntimeError("last error")
  first_one_shot = AsyncMock(side_effect=first_error)
  last_one_shot = AsyncMock(side_effect=last_error)

  with (
      patch(
          _BUILD_BACKEND_PATCH_TARGET,
          side_effect=[make_one_shot_backend(first_one_shot), make_one_shot_backend(last_one_shot)],
      ),
      patch(_EXTRACT_RECAP_PATCH_TARGET, return_value={"asks": [], "last": None}),
      patch(_WRITE_CACHE_ENTRY_PATCH_TARGET) as mock_write,
      pytest.raises(RuntimeError) as exc_info,
  ):
    await generate_and_cache_summary(session_mgr, "s", 5, cfg)

  assert exc_info.value is last_error
  first_one_shot.assert_awaited_once()
  last_one_shot.assert_awaited_once()
  mock_write.assert_not_called()


@pytest.mark.asyncio
async def test_recap_raises_last_exception_after_error_and_empty_result() -> None:
  cfg = CharlieBotConfig(
      backend_options=[
          BackendOption(id="first", label="First", type="cc-claude", model="haiku"),
          BackendOption(id="empty", label="Empty", type="codex", model="gpt-x"),
          BackendOption(id="last", label="Last", type="kimi", model="k2"),
      ],
      model_preference=["first", "empty", "last"],
  )
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = SessionMetadata(id="s", name="Session 1", backend="")
  first_error = RuntimeError("first error")
  last_error = RuntimeError("last error")
  first_one_shot = AsyncMock(side_effect=first_error)
  empty_one_shot = AsyncMock(return_value="")
  last_one_shot = AsyncMock(side_effect=last_error)

  with (
      patch(
          _BUILD_BACKEND_PATCH_TARGET,
          side_effect=[
              make_one_shot_backend(first_one_shot),
              make_one_shot_backend(empty_one_shot),
              make_one_shot_backend(last_one_shot),
          ],
      ),
      patch(_EXTRACT_RECAP_PATCH_TARGET, return_value={"asks": [], "last": None}),
      patch(_WRITE_CACHE_ENTRY_PATCH_TARGET) as mock_write,
      pytest.raises(RuntimeError) as exc_info,
  ):
    await generate_and_cache_summary(session_mgr, "s", 5, cfg)

  assert exc_info.value is last_error
  first_one_shot.assert_awaited_once()
  empty_one_shot.assert_awaited_once()
  last_one_shot.assert_awaited_once()
  mock_write.assert_not_called()
