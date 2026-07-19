"""Focused regression tests for session autonaming/autogrouping."""

import json
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from src.core import autonamer
from src.core.autonamer import _unfaithful_tokens, maybe_auto_name, maybe_auto_name_from_claude_ai_title
from src.core.models import SessionMetadata


def _write_claude_jsonl(home_dir: Path, session_id: str, rows: list[dict | str]) -> Path:
  jsonl_path = home_dir / ".claude" / "projects" / "encoded-cwd" / f"{session_id}.jsonl"
  jsonl_path.parent.mkdir(parents=True)
  lines = [row if isinstance(row, str) else json.dumps(row) for row in rows]
  jsonl_path.write_text("\n".join(lines) + "\n", encoding="utf-8")
  return jsonl_path


@pytest.mark.asyncio
async def test_maybe_auto_name_passes_existing_groups_to_claude() -> None:
  cfg = SimpleNamespace(gemini_api_key=None, gemini_model="gemini-2.5-flash")
  session_meta = SessionMetadata(id="session-1", name="Session 7")
  session_mgr = AsyncMock()
  session_mgr.get_session.side_effect = [
      SessionMetadata(id="session-1", name="Session 7", group=None),
      SessionMetadata(id="session-1", name="7: Test Name", group=None),
  ]

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
  session_mgr.get_session.side_effect = [
      SessionMetadata(id="session-2", name="Session 8", group="Manual"),
      SessionMetadata(id="session-2", name="8: Test Name", group="Manual"),
  ]

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


@pytest.mark.asyncio
async def test_maybe_auto_name_gemini_path_preserves_default_session_number() -> None:
  cfg = SimpleNamespace(gemini_api_key="test-key", gemini_model="gemini-2.5-flash")
  session_meta = SessionMetadata(id="session-gemini-prefix", name="Session 42")
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = SessionMetadata(id="session-gemini-prefix", name="Session 42")
  provider = SimpleNamespace(generate_text=AsyncMock(return_value='{"name":"Refactor Config Loader"}'))

  with (
      patch("src.core.autonamer.GeminiProvider", return_value=provider) as mock_provider,
      patch("src.core.autonamer.streaming_manager.broadcast", new=AsyncMock()),
  ):
    await maybe_auto_name(
        cfg,
        session_meta,
        "Refactor the config loader",
        "I updated the loader to use the shared config parser.",
        session_mgr,
        [],
    )

  mock_provider.assert_called_once_with(api_key="test-key", model="gemini-2.5-flash")
  provider.generate_text.assert_awaited_once()
  session_mgr.rename_session.assert_awaited_once_with("session-gemini-prefix", "42: Refactor Config Loader")


@pytest.mark.asyncio
async def test_maybe_auto_name_rechecks_current_name_before_renaming() -> None:
  cfg = SimpleNamespace(gemini_api_key=None, gemini_model="gemini-2.5-flash")
  session_meta = SessionMetadata(id="session-renamed", name="Session 9")
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = SessionMetadata(id="session-renamed", name="Manual Name")

  with (
      patch(
          "src.core.autonamer._generate_name_via_claude_cli",
          new=AsyncMock(return_value='{"name":"Generated Name","group":"Work"}'),
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

  session_mgr.rename_session.assert_not_awaited()
  session_mgr.set_group.assert_not_awaited()
  mock_broadcast.assert_not_awaited()


@pytest.mark.asyncio
async def test_claude_ai_title_returns_when_no_jsonl_exists(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  home_dir = tmp_path / "home"
  monkeypatch.setattr(autonamer.Path, "home", staticmethod(lambda: home_dir))
  session_mgr = AsyncMock()
  session_meta = SessionMetadata(id="session-no-jsonl", name="Session 1")

  await maybe_auto_name_from_claude_ai_title(session_meta, session_mgr)

  session_mgr.rename_session.assert_not_awaited()


@pytest.mark.asyncio
async def test_claude_ai_title_returns_when_jsonl_has_no_ai_title(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  home_dir = tmp_path / "home"
  monkeypatch.setattr(autonamer.Path, "home", staticmethod(lambda: home_dir))
  session_meta = SessionMetadata(id="session-no-title", name="Session 2")
  session_mgr = AsyncMock()
  _write_claude_jsonl(
      home_dir,
      session_meta.id,
      [
          {"type": "user", "message": {"content": "hello"}},
          "not-json-yet",
          {"type": "assistant", "message": {"content": "hi"}},
      ],
  )

  await maybe_auto_name_from_claude_ai_title(session_meta, session_mgr)

  session_mgr.rename_session.assert_not_awaited()


@pytest.mark.asyncio
async def test_claude_ai_title_applies_title_for_default_session(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  home_dir = tmp_path / "home"
  monkeypatch.setattr(autonamer.Path, "home", staticmethod(lambda: home_dir))
  session_meta = SessionMetadata(id="session-title", name="Session 3")
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = SessionMetadata(id="session-title", name="Session 3")
  _write_claude_jsonl(
      home_dir,
      session_meta.id,
      [
          {"type": "assistant", "message": {"content": "ready"}},
          {"type": "ai-title", "aiTitle": "Investigate TUI Autonaming"},
      ],
  )

  with patch("src.core.autonamer.streaming_manager.broadcast", new=AsyncMock()) as mock_broadcast:
    await maybe_auto_name_from_claude_ai_title(session_meta, session_mgr)

  session_mgr.rename_session.assert_awaited_once_with("session-title", "3: Investigate TUI Autonaming")
  session_mgr.set_group.assert_not_awaited()
  assert [call.args[0] for call in mock_broadcast.await_args_list] == ["session:session-title", "sidebar"]


@pytest.mark.asyncio
async def test_claude_ai_title_prefixes_default_session_number(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  home_dir = tmp_path / "home"
  monkeypatch.setattr(autonamer.Path, "home", staticmethod(lambda: home_dir))
  session_meta = SessionMetadata(id="session-tui-prefix", name="Session 77")
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = SessionMetadata(id="session-tui-prefix", name="Session 77")
  _write_claude_jsonl(
      home_dir,
      session_meta.id,
      [{"type": "ai-title", "aiTitle": "Refactor the config loader"}],
  )

  with patch("src.core.autonamer.streaming_manager.broadcast", new=AsyncMock()):
    await maybe_auto_name_from_claude_ai_title(session_meta, session_mgr)

  session_mgr.rename_session.assert_awaited_once_with(
      "session-tui-prefix",
      "77: Refactor the config loader",
  )


@pytest.mark.asyncio
async def test_claude_ai_title_does_not_overwrite_manual_session_name(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  home_dir = tmp_path / "home"
  monkeypatch.setattr(autonamer.Path, "home", staticmethod(lambda: home_dir))
  session_meta = SessionMetadata(id="session-manual", name="My Custom Name")
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = SessionMetadata(id="session-manual", name="My Custom Name")
  _write_claude_jsonl(
      home_dir,
      session_meta.id,
      [{"type": "ai-title", "aiTitle": "Investigate TUI Autonaming"}],
  )

  with patch("src.core.autonamer.streaming_manager.broadcast", new=AsyncMock()) as mock_broadcast:
    await maybe_auto_name_from_claude_ai_title(session_meta, session_mgr)

  session_mgr.rename_session.assert_not_awaited()
  session_mgr.set_group.assert_not_awaited()
  mock_broadcast.assert_not_awaited()


def test_unfaithful_tokens_faithful_title_passes() -> None:
  source = "We worked on the TRELLIS.2 branch and the structure-pack-r2 spec."
  assert _unfaithful_tokens("TRELLIS.2 structure-pack-r2 review", source) == []


def test_unfaithful_tokens_rejects_invented_mixed_case_digit_token() -> None:
  # "CTRELLIS.2" is not a substring of a source containing only "TRELLIS.2".
  source = "We worked on the TRELLIS.2 branch."
  assert _unfaithful_tokens("CTRELLIS.2 Perf", source) == ["CTRELLIS.2"]


def test_unfaithful_tokens_strips_leading_trailing_punctuation_preserving_interior() -> None:
  # "(TRELLIS.2)" strips to "TRELLIS.2" (interior dot kept) and matches the source.
  source = "We worked on the TRELLIS.2 branch."
  assert _unfaithful_tokens("(TRELLIS.2)", source) == []


def test_unfaithful_tokens_single_capital_ordinary_word_not_checked() -> None:
  # "Perf" has one capital and no digit; an ordinary word, never checked.
  source = "We worked on the project."
  assert _unfaithful_tokens("Perf Review", source) == []


def test_unfaithful_tokens_digit_only_token_not_checked() -> None:
  # The session-number prefix "7:" strips to digit-only "7"; never checked.
  source = "We worked on the project review."
  assert _unfaithful_tokens("7: Project Review", source) == []


@pytest.mark.asyncio
async def test_maybe_auto_name_retries_when_first_title_unfaithful_then_succeeds() -> None:
  cfg = SimpleNamespace(gemini_api_key=None, gemini_model="gemini-2.5-flash")
  session_meta = SessionMetadata(id="session-retry", name="Session 7")
  session_mgr = AsyncMock()
  session_mgr.get_session.side_effect = [
      SessionMetadata(id="session-retry", name="Session 7", group=None),
      SessionMetadata(id="session-retry", name="7: Trellis Review", group=None),
  ]

  with (
      patch(
          "src.core.autonamer._generate_name_via_claude_cli",
          new=AsyncMock(side_effect=[
              '{"name":"CTRELLIS.2 Review","group":"work"}',
              '{"name":"Trellis Review","group":"work"}',
          ]),
      ) as mock_generate,
      patch("src.core.autonamer.streaming_manager.broadcast", new=AsyncMock()),
  ):
    await maybe_auto_name(
        cfg,
        session_meta,
        "Let's review the TRELLIS.2 branch changes.",
        "Here is the review.",
        session_mgr,
        ["Work"],
    )

  session_mgr.rename_session.assert_awaited_once_with("session-retry", "7: Trellis Review")
  session_mgr.set_group.assert_awaited_once_with("session-retry", "Work")
  assert mock_generate.await_count == 2
  first_prompt, _ = mock_generate.await_args_list[0].args
  second_prompt, _ = mock_generate.await_args_list[1].args
  assert "names not present in the conversation" not in first_prompt
  assert "names not present in the conversation: CTRELLIS.2" in second_prompt


@pytest.mark.asyncio
async def test_maybe_auto_name_keeps_default_name_when_both_titles_unfaithful() -> None:
  cfg = SimpleNamespace(gemini_api_key=None, gemini_model="gemini-2.5-flash")
  session_meta = SessionMetadata(id="session-fallback", name="Session 7")
  session_mgr = AsyncMock()

  with (
      patch(
          "src.core.autonamer._generate_name_via_claude_cli",
          new=AsyncMock(side_effect=[
              '{"name":"CTRELLIS.2 Review","group":"work"}',
              '{"name":"WRELLIS.3 Review","group":"work"}',
          ]),
      ),
      patch("src.core.autonamer.streaming_manager.broadcast", new=AsyncMock()) as mock_broadcast,
      patch("src.core.autonamer.log", new=MagicMock()) as mock_log,
  ):
    await maybe_auto_name(
        cfg,
        session_meta,
        "Let's review the TRELLIS.2 branch changes.",
        "Here is the review.",
        session_mgr,
        ["Work"],
    )

  session_mgr.rename_session.assert_not_awaited()
  session_mgr.set_group.assert_not_awaited()
  mock_broadcast.assert_not_awaited()
  mock_log.warning.assert_called_once_with(
      "session_auto_name_unfaithful",
      session_id="session-fallback",
      rejected_title="7: WRELLIS.3 Review",
      tokens=["WRELLIS.3"],
  )
