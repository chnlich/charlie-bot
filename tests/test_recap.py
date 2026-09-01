"""Tests for the recap summary path (generate_and_cache_summary, _write_cache_entry)."""

import json
import os
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
from conftest import append_events as _append_events
from conftest import make_home_session, make_one_shot_backend

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


@pytest.mark.asyncio
async def test_extract_recap_memoizes_repeats_at_one_divider(tmp_path: Path) -> None:
  """A repeat extract at the same divider does not re-scan the on-disk events.

  The chat UI re-requests an open recap panel on every re-materialization;
  without the memo each repeat re-parses the whole event range below the
  divider. A memo that keyed on more than (session_id, divider end) would miss
  on appends beyond the divider even though that range cannot change.
  """
  _cfg, mgr, session = await make_home_session(tmp_path, name="memo")
  _append_events(
      mgr.get_chat_events_path(session.id),
      [
          {"type": "user", "content": "first ask"},
          {"type": "assistant", "message": {"content": [{"type": "text", "text": "first answer"}]}},
      ],
  )

  with patch.object(mgr, "load_chat_events_range", wraps=mgr.load_chat_events_range) as spy:
    divider = recap.extract_recap(mgr, session.id)
    assert spy.call_count == 1
    _append_events(
        mgr.get_chat_events_path(session.id),
        [
            {"type": "user", "content": "second ask"},
            {"type": "assistant", "message": {"content": [{"type": "text", "text": "second answer"}]}},
        ],
    )
    repeat = recap.extract_recap(mgr, session.id, upto=1)
    assert repeat == divider
    assert spy.call_count == 1
    grown = recap.extract_recap(mgr, session.id)
    assert spy.call_count == 2
    assert grown["asks"] == ["first ask", "second ask"]
    assert grown["last"] == {"user": "second ask", "assistant": "second answer"}


@pytest.mark.asyncio
async def test_extract_memo_drops_with_session_runtime_state(tmp_path: Path) -> None:
  """Permanent delete evicts the session's memo entries.

  Slack-thread sessions carry deterministic uuid5 ids and CreateSessionRequest
  accepts pinned ids, so an id can return for a different conversation; without
  the drop the new conversation would be served the deleted one's extraction.
  """
  _cfg, mgr, session = await make_home_session(tmp_path, name="memo-drop")
  _append_events(mgr.get_chat_events_path(session.id), [{"type": "user", "content": "ask"}])

  recap.extract_recap(mgr, session.id)
  assert any(key[0] == session.id for key in recap._extract_memo)
  assert await mgr.delete_session_permanently(session.id)
  assert not any(key[0] == session.id for key in recap._extract_memo)


_REAL_REPLACE = os.replace


@pytest.mark.asyncio
async def test_recap_cache_write_swaps_target_via_os_replace(tmp_path: Path) -> None:
  """The recap summary cache write goes through os.replace on recap_summaries.json.

  The recap GET reads this file from an executor thread with no coordination
  against the summarize write; an in-place truncating write-through lets that
  read observe a half-written file and fail json parsing. A write side that
  never calls os.replace fails here because the hook never fires.
  """
  _cfg, mgr, session = await make_home_session(tmp_path, name="recap")
  target = recap._cache_path(mgr, session.id)

  replaced_targets: list[str] = []

  def _capture_replace(src: str, dst: str) -> None:
    replaced_targets.append(str(dst))
    return _REAL_REPLACE(src, dst)

  with patch("src.core.json_utils.os.replace", side_effect=_capture_replace):
    recap._write_cache_entry(mgr, session.id, 0, "summary")

  assert str(target) in replaced_targets


@pytest.mark.asyncio
async def test_recap_cache_read_at_swap_observes_previous_document(tmp_path: Path) -> None:
  """A read of the cache at the instant of the swap yields the previous complete document.

  An in-place truncating write side would read empty or partial JSON here.
  """
  _cfg, mgr, session = await make_home_session(tmp_path, name="recap")
  target = recap._cache_path(mgr, session.id)
  recap._write_cache_entry(mgr, session.id, 0, "before")

  read_at_swap: list[str] = []

  def _read_then_replace(src: str, dst: str) -> None:
    read_at_swap.append(target.read_text(encoding="utf-8"))
    return _REAL_REPLACE(src, dst)

  with patch("src.core.json_utils.os.replace", side_effect=_read_then_replace):
    recap._write_cache_entry(mgr, session.id, 1, "after")

  assert len(read_at_swap) == 1
  assert json.loads(read_at_swap[0])["0"]["summary"] == "before"
