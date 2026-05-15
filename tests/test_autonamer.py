"""Focused regression tests for session autonaming/autogrouping."""

import json
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import AsyncMock, patch

import pytest

from src.core import autonamer
from src.core.autonamer import maybe_auto_name, maybe_auto_name_from_claude_ai_title
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

  session_mgr.rename_session.assert_awaited_once_with("session-title", "Investigate TUI Autonaming")
  session_mgr.set_group.assert_not_awaited()
  assert [call.args[0] for call in mock_broadcast.await_args_list] == ["session:session-title", "sidebar"]


@pytest.mark.asyncio
async def test_claude_ai_title_does_not_overwrite_manual_session_name(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  home_dir = tmp_path / "home"
  monkeypatch.setattr(autonamer.Path, "home", staticmethod(lambda: home_dir))
  session_meta = SessionMetadata(id="session-manual", name="My Custom Name")
  session_mgr = AsyncMock()
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
