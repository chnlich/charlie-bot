"""Focused regression tests for session autonaming/autogrouping."""

import json
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from src.core import autonamer
from src.core.autonamer import (
    _unfaithful_tokens, maybe_auto_name, maybe_auto_name_from_claude_ai_title, select_light_backend)
from src.core.config import CharlieBotConfig
from src.core.models import BackendOption, SessionMetadata


def _write_claude_jsonl(home_dir: Path, session_id: str, rows: list[dict | str]) -> Path:
  jsonl_path = home_dir / ".claude" / "projects" / "encoded-cwd" / f"{session_id}.jsonl"
  jsonl_path.parent.mkdir(parents=True)
  lines = [row if isinstance(row, str) else json.dumps(row) for row in rows]
  jsonl_path.write_text("\n".join(lines) + "\n", encoding="utf-8")
  return jsonl_path


def _cc_cfg() -> CharlieBotConfig:
  """Config whose only backend is a light cc-claude option, also in model_preference."""
  return CharlieBotConfig(
      backend_options=[BackendOption(id="light-cc", label="Light CC", type="cc-claude", model="haiku")],
      model_preference=["light-cc"],
  )


def _mock_backend(one_shot: AsyncMock) -> MagicMock:
  """A stand-in backend whose one_shot_text is the given AsyncMock."""
  backend = MagicMock()
  backend.one_shot_text = one_shot
  return backend


class _FakeStdout:
  """Async iterator over canned NDJSON byte lines, mimicking proc.stdout."""

  def __init__(self, lines: list[bytes]) -> None:
    self._lines = list(lines)

  def __aiter__(self) -> "_FakeStdout":
    return self

  async def __anext__(self) -> bytes:
    if not self._lines:
      raise StopAsyncIteration
    return self._lines.pop(0)


# ---------------------------------------------------------------------------
# maybe_auto_name — light-backend one-shot path
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_maybe_auto_name_passes_existing_groups_to_backend() -> None:
  cfg = _cc_cfg()
  session_meta = SessionMetadata(id="session-1", name="Session 7", backend="light-cc")
  session_mgr = AsyncMock()
  session_mgr.get_session.side_effect = [
      SessionMetadata(id="session-1", name="Session 7", group=None),
      SessionMetadata(id="session-1", name="7: Test Name", group=None),
  ]
  one_shot = AsyncMock(return_value='{"name":"Test Name","group":"work"}')

  with (
      patch("src.core.autonamer.build_backend", return_value=_mock_backend(one_shot)),
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

  prompt, system_prompt = one_shot.await_args.args
  assert "Prefer reusing one of these existing groups: [Work, Personal]" in prompt
  assert "Prefer reusing one of these existing groups: [Work, Personal]" in system_prompt
  session_mgr.set_group.assert_awaited_once_with("session-1", "Work")


@pytest.mark.asyncio
async def test_maybe_auto_name_does_not_overwrite_existing_manual_group() -> None:
  cfg = _cc_cfg()
  session_meta = SessionMetadata(id="session-2", name="Session 8", backend="light-cc")
  session_mgr = AsyncMock()
  session_mgr.get_session.side_effect = [
      SessionMetadata(id="session-2", name="Session 8", group="Manual"),
      SessionMetadata(id="session-2", name="8: Test Name", group="Manual"),
  ]
  one_shot = AsyncMock(return_value='{"name":"Test Name","group":"Work"}')

  with (
      patch("src.core.autonamer.build_backend", return_value=_mock_backend(one_shot)),
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
async def test_maybe_auto_name_preserves_default_session_number() -> None:
  cfg = _cc_cfg()
  session_meta = SessionMetadata(id="session-prefix", name="Session 42", backend="light-cc")
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = SessionMetadata(id="session-prefix", name="Session 42")
  one_shot = AsyncMock(return_value='{"name":"Refactor Config Loader"}')

  with (
      patch("src.core.autonamer.build_backend", return_value=_mock_backend(one_shot)),
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

  one_shot.assert_awaited_once()
  session_mgr.rename_session.assert_awaited_once_with("session-prefix", "42: Refactor Config Loader")


@pytest.mark.asyncio
async def test_maybe_auto_name_rechecks_current_name_before_renaming() -> None:
  cfg = _cc_cfg()
  session_meta = SessionMetadata(id="session-renamed", name="Session 9", backend="light-cc")
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = SessionMetadata(id="session-renamed", name="Manual Name")
  one_shot = AsyncMock(return_value='{"name":"Generated Name","group":"Work"}')

  with (
      patch("src.core.autonamer.build_backend", return_value=_mock_backend(one_shot)),
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
async def test_maybe_auto_name_retries_when_first_title_unfaithful_then_succeeds() -> None:
  cfg = _cc_cfg()
  session_meta = SessionMetadata(id="session-retry", name="Session 7", backend="light-cc")
  session_mgr = AsyncMock()
  session_mgr.get_session.side_effect = [
      SessionMetadata(id="session-retry", name="Session 7", group=None),
      SessionMetadata(id="session-retry", name="7: Trellis Review", group=None),
  ]
  one_shot = AsyncMock(
      side_effect=[
          '{"name":"CTRELLIS.2 Review","group":"work"}',
          '{"name":"Trellis Review","group":"work"}',
      ])

  with (
      patch("src.core.autonamer.build_backend", return_value=_mock_backend(one_shot)),
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
  assert one_shot.await_count == 2
  first_prompt, _ = one_shot.await_args_list[0].args
  second_prompt, _ = one_shot.await_args_list[1].args
  assert "names not present in the conversation" not in first_prompt
  assert "names not present in the conversation: CTRELLIS.2" in second_prompt


@pytest.mark.asyncio
async def test_maybe_auto_name_keeps_default_name_when_both_titles_unfaithful() -> None:
  cfg = _cc_cfg()
  session_meta = SessionMetadata(id="session-fallback", name="Session 7", backend="light-cc")
  session_mgr = AsyncMock()
  one_shot = AsyncMock(
      side_effect=[
          '{"name":"CTRELLIS.2 Review","group":"work"}',
          '{"name":"WRELLIS.3 Review","group":"work"}',
      ])

  with (
      patch("src.core.autonamer.build_backend", return_value=_mock_backend(one_shot)),
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


@pytest.mark.asyncio
async def test_maybe_auto_name_keeps_default_metadata_when_backend_fails_and_retries() -> None:
  cfg = _cc_cfg()
  session_meta = SessionMetadata(id="session-backend-failure", name="Session 10", backend="light-cc")
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = SessionMetadata(
      id="session-backend-failure", name="Session 10", group=None)
  one_shot = AsyncMock(
      side_effect=[
          RuntimeError("unsupported reasoning effort"),
          '{"name":"Recovered Title","group":"Recovered"}',
      ])

  with (
      patch("src.core.autonamer.build_backend", return_value=_mock_backend(one_shot)),
      patch("src.core.autonamer.streaming_manager.broadcast", new=AsyncMock()) as mock_broadcast,
      patch("src.core.autonamer.log", new=MagicMock()) as mock_log,
  ):
    await maybe_auto_name(cfg, session_meta, "First naming attempt", "Backend failed.", session_mgr, [])

    session_mgr.rename_session.assert_not_awaited()
    session_mgr.set_group.assert_not_awaited()
    mock_broadcast.assert_not_awaited()
    mock_log.warning.assert_called_once_with(
        "autonamer_failed",
        session_id="session-backend-failure",
        error="unsupported reasoning effort",
    )

    await maybe_auto_name(cfg, session_meta, "Retry naming attempt", "Backend recovered.", session_mgr, [])

  session_mgr.rename_session.assert_awaited_once_with("session-backend-failure", "10: Recovered Title")
  session_mgr.set_group.assert_awaited_once_with("session-backend-failure", "Recovered")
  assert one_shot.await_count == 2


# ---------------------------------------------------------------------------
# select_light_backend — same-type-first selection
# ---------------------------------------------------------------------------


def test_select_light_backend_same_type_different_id_wins() -> None:
  cfg = CharlieBotConfig(
      backend_options=[
          BackendOption(id="codex-session", label="Session", type="codex", model="gpt-5.5"),
          BackendOption(id="codex-gpt-5.6-luna-personal", label="Luna", type="codex", model="gpt-5.6-luna"),
      ],
      model_preference=["codex-gpt-5.6-luna-personal"],
  )
  option = select_light_backend(cfg, "codex-session")
  assert option is not None
  assert option.id == "codex-gpt-5.6-luna-personal"
  assert option.model == "gpt-5.6-luna"


def test_select_light_backend_same_id_is_selectable() -> None:
  cfg = CharlieBotConfig(
      backend_options=[BackendOption(id="opencode-glm52", label="OC", type="opencode", model="prov/model")],
      model_preference=["opencode-glm52"],
  )
  option = select_light_backend(cfg, "opencode-glm52")
  assert option is not None
  assert option.id == "opencode-glm52"


def test_select_light_backend_returns_none_when_no_same_type() -> None:
  cfg = CharlieBotConfig(
      backend_options=[
          BackendOption(id="codex-session", label="Session", type="codex", model="gpt-5.5"),
          BackendOption(id="claude-haiku", label="Haiku", type="cc-claude", model="haiku"),
      ],
      model_preference=["claude-haiku"],
  )
  assert select_light_backend(cfg, "codex-session") is None


def test_select_light_backend_returns_none_for_unknown_session_backend() -> None:
  cfg = CharlieBotConfig(
      backend_options=[BackendOption(id="claude-haiku", label="Haiku", type="cc-claude", model="haiku")],
      model_preference=["claude-haiku"],
  )
  assert select_light_backend(cfg, "does-not-exist") is None


@pytest.mark.asyncio
async def test_maybe_auto_name_builds_same_type_luna_backend() -> None:
  cfg = CharlieBotConfig(
      backend_options=[
          BackendOption(id="codex-session", label="Session", type="codex", model="gpt-5.5"),
          BackendOption(id="codex-gpt-5.6-luna-personal", label="Luna", type="codex", model="gpt-5.6-luna"),
      ],
      model_preference=["codex-gpt-5.6-luna-personal"],
  )
  session_meta = SessionMetadata(id="session-luna", name="Session 5", backend="codex-session")
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = SessionMetadata(id="session-luna", name="Session 5")
  one_shot = AsyncMock(return_value='{"name":"Codex Title"}')

  with (
      patch("src.core.autonamer.build_backend", return_value=_mock_backend(one_shot)) as mock_build,
      patch("src.core.autonamer.streaming_manager.broadcast", new=AsyncMock()),
  ):
    await maybe_auto_name(cfg, session_meta, "Do the codex thing", "Done.", session_mgr, [])

  built_option = mock_build.call_args.args[0]
  assert built_option.id == "codex-gpt-5.6-luna-personal"
  assert built_option.model == "gpt-5.6-luna"
  one_shot.assert_awaited_once()
  session_mgr.rename_session.assert_awaited_once_with("session-luna", "5: Codex Title")


@pytest.mark.asyncio
async def test_maybe_auto_name_builds_same_id_opencode_backend() -> None:
  cfg = CharlieBotConfig(
      backend_options=[BackendOption(id="opencode-glm52", label="OC", type="opencode", model="prov/model")],
      model_preference=["opencode-glm52"],
  )
  session_meta = SessionMetadata(id="session-oc", name="Session 6", backend="opencode-glm52")
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = SessionMetadata(id="session-oc", name="Session 6")
  one_shot = AsyncMock(return_value='{"name":"Open Code Task"}')

  with (
      patch("src.core.autonamer.build_backend", return_value=_mock_backend(one_shot)) as mock_build,
      patch("src.core.autonamer.streaming_manager.broadcast", new=AsyncMock()),
  ):
    await maybe_auto_name(cfg, session_meta, "Use opencode", "Done.", session_mgr, [])

  built_option = mock_build.call_args.args[0]
  assert built_option.id == "opencode-glm52"
  one_shot.assert_awaited_once()


@pytest.mark.asyncio
async def test_maybe_auto_name_skips_loudly_when_no_same_type_preference() -> None:
  cfg = CharlieBotConfig(
      backend_options=[
          BackendOption(id="codex-session", label="Session", type="codex", model="gpt-5.5"),
          BackendOption(id="claude-haiku", label="Haiku", type="cc-claude", model="haiku"),
      ],
      model_preference=["claude-haiku"],
  )
  session_meta = SessionMetadata(id="session-skip", name="Session 3", backend="codex-session")
  session_mgr = AsyncMock()

  with (
      patch("src.core.autonamer.build_backend") as mock_build,
      patch("src.core.autonamer.streaming_manager.broadcast", new=AsyncMock()) as mock_broadcast,
      patch("src.core.autonamer.log", new=MagicMock()) as mock_log,
  ):
    await maybe_auto_name(cfg, session_meta, "Some ask", "Some answer.", session_mgr, [])

  mock_build.assert_not_called()
  session_mgr.rename_session.assert_not_awaited()
  session_mgr.set_group.assert_not_awaited()
  mock_broadcast.assert_not_awaited()
  mock_log.warning.assert_called_once_with(
      "autonamer_skipped",
      reason="no_same_type_preference",
      session_id="session-skip",
      backend="codex-session",
  )


# ---------------------------------------------------------------------------
# Claude ai-title strategy (TUI path) — unchanged behavior
# ---------------------------------------------------------------------------


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
          {
              "type": "user",
              "message": {
                  "content": "hello"
              }
          },
          "not-json-yet",
          {
              "type": "assistant",
              "message": {
                  "content": "hi"
              }
          },
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
          {
              "type": "assistant",
              "message": {
                  "content": "ready"
              }
          },
          {
              "type": "ai-title",
              "aiTitle": "Investigate TUI Autonaming"
          },
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
      [{
          "type": "ai-title",
          "aiTitle": "Refactor the config loader"
      }],
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
      [{
          "type": "ai-title",
          "aiTitle": "Investigate TUI Autonaming"
      }],
  )

  with patch("src.core.autonamer.streaming_manager.broadcast", new=AsyncMock()) as mock_broadcast:
    await maybe_auto_name_from_claude_ai_title(session_meta, session_mgr)

  session_mgr.rename_session.assert_not_awaited()
  session_mgr.set_group.assert_not_awaited()
  mock_broadcast.assert_not_awaited()


# ---------------------------------------------------------------------------
# _unfaithful_tokens — pure fidelity checks
# ---------------------------------------------------------------------------


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


# ---------------------------------------------------------------------------
# Direct one_shot_text overrides (stubbed subprocess)
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_claude_one_shot_text_uses_model_and_returns_stdout() -> None:
  from src.agents.backends.claude_code import ClaudeCodeBackend

  proc = MagicMock()
  proc.communicate = AsyncMock(return_value=(b'{"name":"Refactor Loader"}\n', b""))
  proc.returncode = 0
  proc.pid = 4321

  with patch("asyncio.create_subprocess_exec", new=AsyncMock(return_value=proc)) as mock_exec:
    backend = ClaudeCodeBackend(model="haiku")
    result = await backend.one_shot_text("the prompt", "the system prompt", timeout=5.0)

  assert result == '{"name":"Refactor Loader"}'
  args = mock_exec.await_args.args
  assert args[0] == "claude"
  assert args[args.index("--model") + 1] == "haiku"
  assert args[args.index("--system-prompt") + 1] == "the system prompt"
  assert "--disallowed-tools" in args
  assert proc.communicate.await_args.kwargs["input"] == b"the prompt"


@pytest.mark.asyncio
async def test_claude_one_shot_text_raises_on_nonzero_exit() -> None:
  from src.agents.backends.claude_code import ClaudeCodeBackend

  proc = MagicMock()
  proc.communicate = AsyncMock(return_value=(b"", b"boom"))
  proc.returncode = 2
  proc.pid = 4322

  with patch("asyncio.create_subprocess_exec", new=AsyncMock(return_value=proc)):
    backend = ClaudeCodeBackend(model="haiku")
    with pytest.raises(RuntimeError, match="claude CLI failed"):
      await backend.one_shot_text("p", "s", timeout=5.0)


@pytest.mark.asyncio
async def test_codex_one_shot_text_accumulates_agent_message() -> None:
  from src.agents.backends.codex import CodexBackend

  lines = [
      b'{"type":"thread.started","thread_id":"t1"}\n',
      b'{"type":"item.started","item":{"type":"agent_message","id":"a1","text":""}}\n',
      b'{"type":"item.completed","item":{"type":"agent_message","id":"a1","text":"OK title"}}\n',
      b'{"type":"turn.completed","usage":{}}\n',
  ]
  proc = MagicMock()
  proc.stdout = _FakeStdout(lines)
  proc.stderr = MagicMock()
  proc.stderr.read = AsyncMock(return_value=b"")
  proc.wait = AsyncMock(return_value=0)
  proc.pid = 9999
  proc.returncode = 0

  with patch("asyncio.create_subprocess_exec", new=AsyncMock(return_value=proc)) as mock_exec:
    backend = CodexBackend(model="gpt-x", model_reasoning_effort="high")
    result = await backend.one_shot_text("hello prompt", "sys prompt", timeout=5.0)

  assert result == "OK title"
  args = mock_exec.await_args.args
  assert "exec" in args and "--json" in args
  assert args[args.index("--model") + 1] == "gpt-x"
  assert 'model_reasoning_effort="high"' in args
  # Codex has no system-prompt flag: it is framed into the final (post "--") prompt arg.
  assert args[-1] == "<system-instructions>\nsys prompt\n</system-instructions>\n\nhello prompt"
  proc.wait.assert_awaited()


@pytest.mark.asyncio
async def test_codex_one_shot_text_returns_empty_when_no_agent_message() -> None:
  from src.agents.backends.codex import CodexBackend

  lines = [
      b'{"type":"thread.started","thread_id":"t1"}\n',
      b'{"type":"turn.completed","usage":{}}\n',
  ]
  proc = MagicMock()
  proc.stdout = _FakeStdout(lines)
  proc.stderr = MagicMock()
  proc.stderr.read = AsyncMock(return_value=b"")
  proc.wait = AsyncMock(return_value=0)
  proc.pid = 9998
  proc.returncode = 0

  with patch("asyncio.create_subprocess_exec", new=AsyncMock(return_value=proc)):
    backend = CodexBackend(model="gpt-x")
    with pytest.raises(RuntimeError, match="no assistant text"):
      await backend.one_shot_text("hi", "sys", timeout=5.0)


@pytest.mark.asyncio
async def test_opencode_one_shot_text_extracts_text_from_flat_part_event(monkeypatch) -> None:
  """`opencode run --format json` emits flat part-shaped events
  ({"type":"text","part":{...}}), not the SSE-bus shape serve uses."""
  from src.agents.backends.opencode import OpenCodeBackend

  monkeypatch.setattr(
      "src.agents.backends.opencode.resolve_binary",
      lambda name, fallback: "/usr/bin/opencode",
  )
  lines = [
      b'{"type":"step_start","timestamp":1,"sessionID":"s1","part":{"id":"prt_a","messageID":"m1","sessionID":"s1","type":"step-start"}}\n',
      b'{"type":"text","timestamp":2,"sessionID":"s1","part":{"id":"prt_b","messageID":"m1","sessionID":"s1","type":"text","text":"OK title","time":{"start":1,"end":2}}}\n',
      b'{"type":"step_finish","timestamp":3,"sessionID":"s1","part":{"id":"prt_c","reason":"stop","messageID":"m1","sessionID":"s1","type":"step-finish","tokens":{"total":1,"input":1,"output":1,"reasoning":0,"cache":{"write":0,"read":0}},"cost":0}}\n',
  ]
  proc = MagicMock()
  proc.stdout = _FakeStdout(lines)
  proc.stderr = MagicMock()
  proc.stderr.read = AsyncMock(return_value=b"")
  proc.wait = AsyncMock(return_value=0)
  proc.pid = 7777

  with patch("asyncio.create_subprocess_exec", new=AsyncMock(return_value=proc)) as mock_exec:
    backend = OpenCodeBackend(model="meshy-sglang-glm52/nvidia/GLM-5.2-NVFP4")
    result = await backend.one_shot_text("hello prompt", "sys prompt", timeout=5.0)

  assert result == "OK title"
  args = mock_exec.await_args.args
  assert args[0] == "/usr/bin/opencode"
  assert "run" in args and "--format" in args and "json" in args
  assert args[args.index("-m") + 1] == "meshy-sglang-glm52/nvidia/GLM-5.2-NVFP4"
  # opencode run has no system-prompt flag: it is framed into the final (post "--") prompt arg.
  assert args[-1] == "<system-instructions>\nsys prompt\n</system-instructions>\n\nhello prompt"
  proc.wait.assert_awaited()


@pytest.mark.asyncio
async def test_opencode_one_shot_text_returns_empty_when_no_text_part(monkeypatch) -> None:
  from src.agents.backends.opencode import OpenCodeBackend

  monkeypatch.setattr(
      "src.agents.backends.opencode.resolve_binary",
      lambda name, fallback: "/usr/bin/opencode",
  )
  lines = [
      b'{"type":"step_start","timestamp":1,"sessionID":"s1","part":{"id":"prt_a","messageID":"m1","sessionID":"s1","type":"step-start"}}\n',
      b'{"type":"step_finish","timestamp":3,"sessionID":"s1","part":{"id":"prt_c","reason":"stop","messageID":"m1","sessionID":"s1","type":"step-finish","tokens":{"total":1,"input":1,"output":1,"reasoning":0,"cache":{"write":0,"read":0}},"cost":0}}\n',
  ]
  proc = MagicMock()
  proc.stdout = _FakeStdout(lines)
  proc.stderr = MagicMock()
  proc.stderr.read = AsyncMock(return_value=b"")
  proc.wait = AsyncMock(return_value=0)
  proc.pid = 7776

  with patch("asyncio.create_subprocess_exec", new=AsyncMock(return_value=proc)):
    backend = OpenCodeBackend(model="meshy-sglang-glm52/nvidia/GLM-5.2-NVFP4")
    result = await backend.one_shot_text("hi", "sys", timeout=5.0)

  assert result == ""
