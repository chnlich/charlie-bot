"""Focused regression tests for session autonaming/autogrouping."""

import json
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
from conftest import SYNTHETIC_MODEL, FakeStdout, make_one_shot_backend

from src.core import autonamer
from src.core.autonamer import (
  iter_light_backends,
  maybe_auto_name,
  maybe_auto_name_from_claude_ai_title,
)
from src.core.config import CharlieBotConfig
from src.core.models import BackendOption, SessionMetadata

_BUILD_BACKEND_PATCH_TARGET = "src.core.autonamer.build_backend"
_STREAMING_BROADCAST_PATCH_TARGET = "src.core.autonamer.streaming_manager.broadcast"
_LOG_PATCH_TARGET = "src.core.autonamer.log"


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


def _fallback_chain_cfg() -> CharlieBotConfig:
  """Config whose model_preference chains a cc-claude first backend onto a codex second backend."""
  return CharlieBotConfig(
      backend_options=[
          BackendOption(id="first-backend", label="First", type="cc-claude", model="haiku"),
          BackendOption(id="second-backend", label="Second", type="codex", model="gpt-x"),
      ],
      model_preference=["first-backend", "second-backend"],
  )


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
      patch(_BUILD_BACKEND_PATCH_TARGET, return_value=make_one_shot_backend(one_shot)),
      patch(_STREAMING_BROADCAST_PATCH_TARGET, new=AsyncMock()),
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
      patch(_BUILD_BACKEND_PATCH_TARGET, return_value=make_one_shot_backend(one_shot)),
      patch(_STREAMING_BROADCAST_PATCH_TARGET, new=AsyncMock()) as mock_broadcast,
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
      patch(_BUILD_BACKEND_PATCH_TARGET, return_value=make_one_shot_backend(one_shot)),
      patch(_STREAMING_BROADCAST_PATCH_TARGET, new=AsyncMock()),
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
      patch(_BUILD_BACKEND_PATCH_TARGET, return_value=make_one_shot_backend(one_shot)),
      patch(_STREAMING_BROADCAST_PATCH_TARGET, new=AsyncMock()) as mock_broadcast,
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
async def test_maybe_auto_name_keeps_snake_case_identifier_verbatim() -> None:
  cfg = _cc_cfg()
  session_meta = SessionMetadata(id="session-snake", name="Session 7", backend="light-cc")
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = SessionMetadata(id="session-snake", name="Session 7")
  raw = '{"name":"CHARLIEBOT_HOME cleanup"}'
  one_shot = AsyncMock(return_value=raw)

  with (
      patch(_BUILD_BACKEND_PATCH_TARGET, return_value=make_one_shot_backend(one_shot)),
      patch(_STREAMING_BROADCAST_PATCH_TARGET, new=AsyncMock()),
  ):
    await maybe_auto_name(
        cfg, session_meta, "Set CHARLIEBOT_HOME for the run.", "Done.", session_mgr, [])

  expected_name = json.loads(raw)["name"]
  session_mgr.rename_session.assert_awaited_once_with("session-snake", f"7: {expected_name}")


@pytest.mark.asyncio
async def test_maybe_auto_name_keeps_chinese_title_verbatim() -> None:
  cfg = _cc_cfg()
  session_meta = SessionMetadata(id="session-cjk", name="Session 8", backend="light-cc")
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = SessionMetadata(id="session-cjk", name="Session 8")
  raw = '{"name":"「TRELLIS.2」分支重构"}'
  one_shot = AsyncMock(return_value=raw)

  with (
      patch(_BUILD_BACKEND_PATCH_TARGET, return_value=make_one_shot_backend(one_shot)),
      patch(_STREAMING_BROADCAST_PATCH_TARGET, new=AsyncMock()),
  ):
    await maybe_auto_name(
        cfg, session_meta, "重构「TRELLIS.2」的分支。", "好的，开始重构。", session_mgr, [])

  expected_name = json.loads(raw)["name"]
  session_mgr.rename_session.assert_awaited_once_with("session-cjk", f"8: {expected_name}")


@pytest.mark.asyncio
async def test_maybe_auto_name_falls_back_after_first_backend_failure() -> None:
  cfg = _fallback_chain_cfg()
  session_meta = SessionMetadata(id="session-backend-failure", name="Session 10", backend="first-backend")
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = SessionMetadata(id="session-backend-failure", name="Session 10", group=None)
  one_shot = AsyncMock(return_value='{"name":"Recovered Title","group":"Recovered"}')

  with (
      patch(
          _BUILD_BACKEND_PATCH_TARGET,
          side_effect=[RuntimeError("unsupported reasoning effort"), make_one_shot_backend(one_shot)],
      ) as mock_build,
      patch(_STREAMING_BROADCAST_PATCH_TARGET, new=AsyncMock()) as mock_broadcast,
      patch(_LOG_PATCH_TARGET, new=MagicMock()) as mock_log,
  ):
    await maybe_auto_name(cfg, session_meta, "First naming attempt", "Backend failed.", session_mgr, [])

  session_mgr.rename_session.assert_awaited_once_with("session-backend-failure", "10: Recovered Title")
  session_mgr.set_group.assert_awaited_once_with("session-backend-failure", "Recovered")
  assert [call.args[0].id for call in mock_build.call_args_list] == ["first-backend", "second-backend"]
  one_shot.assert_awaited_once()
  mock_log.warning.assert_called_once_with(
      "autonamer_failed",
      session_id="session-backend-failure",
      error="unsupported reasoning effort",
  )
  assert mock_broadcast.await_count == 2


@pytest.mark.asyncio
async def test_maybe_auto_name_keeps_default_name_when_all_first_responses_are_unusable() -> None:
  cfg = _fallback_chain_cfg()
  session_meta = SessionMetadata(id="session-exhausted", name="Session 11", backend="first-backend")
  session_mgr = AsyncMock()
  first_one_shot = AsyncMock(return_value="")
  second_one_shot = AsyncMock(return_value='{"group":"only"}')

  with (
      patch(
          _BUILD_BACKEND_PATCH_TARGET,
          side_effect=[make_one_shot_backend(first_one_shot), make_one_shot_backend(second_one_shot)],
      ) as mock_build,
      patch(_STREAMING_BROADCAST_PATCH_TARGET, new=AsyncMock()) as mock_broadcast,
      patch(_LOG_PATCH_TARGET, new=MagicMock()) as mock_log,
  ):
    await maybe_auto_name(cfg, session_meta, "Some ask", "Some answer.", session_mgr, [])

  assert [call.args[0].id for call in mock_build.call_args_list] == ["first-backend", "second-backend"]
  first_one_shot.assert_awaited_once()
  second_one_shot.assert_awaited_once()
  session_mgr.rename_session.assert_not_awaited()
  session_mgr.set_group.assert_not_awaited()
  mock_broadcast.assert_not_awaited()
  assert mock_log.warning.call_count == 2
  assert all(call.args[0] == "autonamer_failed" for call in mock_log.warning.call_args_list)


@pytest.mark.asyncio
async def test_maybe_auto_name_falls_back_to_next_backend_on_non_json_response() -> None:
  cfg = _fallback_chain_cfg()
  session_meta = SessionMetadata(id="session-fallback", name="Session 9", backend="first-backend")
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = SessionMetadata(id="session-fallback", name="Session 9")
  first_one_shot = AsyncMock(return_value="Sure, here's a title: Refactoring the Loader")
  second_raw = '{"name":"Loader Refactor"}'
  second_one_shot = AsyncMock(return_value=second_raw)

  with (
      patch(
          _BUILD_BACKEND_PATCH_TARGET,
          side_effect=[make_one_shot_backend(first_one_shot), make_one_shot_backend(second_one_shot)],
      ) as mock_build,
      patch(_STREAMING_BROADCAST_PATCH_TARGET, new=AsyncMock()),
  ):
    await maybe_auto_name(cfg, session_meta, "Refactor the loader", "Done.", session_mgr, [])

  assert [call.args[0].id for call in mock_build.call_args_list] == ["first-backend", "second-backend"]
  first_one_shot.assert_awaited_once()
  second_one_shot.assert_awaited_once()
  expected_name = json.loads(second_raw)["name"]
  session_mgr.rename_session.assert_awaited_once_with("session-fallback", f"9: {expected_name}")


@pytest.mark.asyncio
async def test_maybe_auto_name_falls_back_to_next_backend_when_name_too_long() -> None:
  cfg = _fallback_chain_cfg()
  session_meta = SessionMetadata(id="session-long", name="Session 10", backend="first-backend")
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = SessionMetadata(id="session-long", name="Session 10")
  first_one_shot = AsyncMock(return_value=json.dumps({"name": "x" * 61}))
  second_raw = '{"name":"Short Title"}'
  second_one_shot = AsyncMock(return_value=second_raw)

  with (
      patch(
          _BUILD_BACKEND_PATCH_TARGET,
          side_effect=[make_one_shot_backend(first_one_shot), make_one_shot_backend(second_one_shot)],
      ) as mock_build,
      patch(_STREAMING_BROADCAST_PATCH_TARGET, new=AsyncMock()),
  ):
    await maybe_auto_name(cfg, session_meta, "Some ask", "Some answer.", session_mgr, [])

  assert [call.args[0].id for call in mock_build.call_args_list] == ["first-backend", "second-backend"]
  first_one_shot.assert_awaited_once()
  second_one_shot.assert_awaited_once()
  expected_name = json.loads(second_raw)["name"]
  session_mgr.rename_session.assert_awaited_once_with("session-long", f"10: {expected_name}")


# ---------------------------------------------------------------------------
# iter_light_backends — ordered resolved preference iteration
# ---------------------------------------------------------------------------


def test_iter_light_backends_preserves_cross_type_preference_order() -> None:
  cfg = CharlieBotConfig(
      backend_options=[
          BackendOption(id="claude", label="Claude", type="cc-claude", model="haiku"),
          BackendOption(id="codex", label="Codex", type="codex", model="gpt-x"),
          BackendOption(id="kimi", label="Kimi", type="kimi", model="k2"),
      ],
      model_preference=["codex", "claude", "kimi"],
  )
  assert [option.id for option in iter_light_backends(cfg)] == ["codex", "claude", "kimi"]


def test_iter_light_backends_skips_unresolved_ids_and_duplicates() -> None:
  cfg = CharlieBotConfig(
      backend_options=[
          BackendOption(id="claude", label="Claude", type="cc-claude", model="haiku"),
          BackendOption(id="codex", label="Codex", type="codex", model="gpt-x"),
      ],
      model_preference=["missing", "claude", "claude", "codex", "missing"],
  )
  assert [option.id for option in iter_light_backends(cfg)] == ["claude", "codex"]


@pytest.mark.asyncio
async def test_maybe_auto_name_builds_codex_backend_for_claude_session() -> None:
  cfg = CharlieBotConfig(
      backend_options=[
          BackendOption(id="claude-session", label="Session", type="cc-claude", model="haiku"),
          BackendOption(id="codex-gpt-5.6-luna-personal", label="Luna", type="codex", model="gpt-5.6-luna"),
      ],
      model_preference=["codex-gpt-5.6-luna-personal"],
  )
  session_meta = SessionMetadata(id="session-luna", name="Session 5", backend="claude-session")
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = SessionMetadata(id="session-luna", name="Session 5")
  one_shot = AsyncMock(return_value='{"name":"Codex Title"}')

  with (
      patch(_BUILD_BACKEND_PATCH_TARGET, return_value=make_one_shot_backend(one_shot)) as mock_build,
      patch(_STREAMING_BROADCAST_PATCH_TARGET, new=AsyncMock()),
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
      patch(_BUILD_BACKEND_PATCH_TARGET, return_value=make_one_shot_backend(one_shot)) as mock_build,
      patch(_STREAMING_BROADCAST_PATCH_TARGET, new=AsyncMock()),
  ):
    await maybe_auto_name(cfg, session_meta, "Use opencode", "Done.", session_mgr, [])

  built_option = mock_build.call_args.args[0]
  assert built_option.id == "opencode-glm52"
  one_shot.assert_awaited_once()


@pytest.mark.asyncio
async def test_maybe_auto_name_skips_loudly_when_no_preference_resolves() -> None:
  cfg = CharlieBotConfig(
      backend_options=[
          BackendOption(id="claude-session", label="Session", type="cc-claude", model="haiku"),
      ],
      model_preference=["does-not-exist"],
  )
  session_meta = SessionMetadata(id="session-skip", name="Session 3", backend="claude-session")
  session_mgr = AsyncMock()

  with (
      patch(_BUILD_BACKEND_PATCH_TARGET) as mock_build,
      patch(_STREAMING_BROADCAST_PATCH_TARGET, new=AsyncMock()) as mock_broadcast,
      patch(_LOG_PATCH_TARGET, new=MagicMock()) as mock_log,
  ):
    await maybe_auto_name(cfg, session_meta, "Some ask", "Some answer.", session_mgr, [])

  mock_build.assert_not_called()
  session_mgr.rename_session.assert_not_awaited()
  session_mgr.set_group.assert_not_awaited()
  mock_broadcast.assert_not_awaited()
  mock_log.warning.assert_called_once_with(
      "autonamer_skipped",
      reason="no_resolvable_preference",
      session_id="session-skip",
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

  with patch(_STREAMING_BROADCAST_PATCH_TARGET, new=AsyncMock()) as mock_broadcast:
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

  with patch(_STREAMING_BROADCAST_PATCH_TARGET, new=AsyncMock()):
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

  with patch(_STREAMING_BROADCAST_PATCH_TARGET, new=AsyncMock()) as mock_broadcast:
    await maybe_auto_name_from_claude_ai_title(session_meta, session_mgr)

  session_mgr.rename_session.assert_not_awaited()
  session_mgr.set_group.assert_not_awaited()
  mock_broadcast.assert_not_awaited()


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
async def test_codex_one_shot_text_accumulates_agent_message(monkeypatch) -> None:
  monkeypatch.setattr(
      "src.agents.backends.codex.resolve_binary",
      lambda name, fallback: "/usr/bin/codex",
  )
  from src.agents.backends.codex import CodexBackend

  lines = [
      b'{"type":"thread.started","thread_id":"t1"}\n',
      b'{"type":"item.started","item":{"type":"agent_message","id":"a1","text":""}}\n',
      b'{"type":"item.completed","item":{"type":"agent_message","id":"a1","text":"OK title"}}\n',
      b'{"type":"turn.completed","usage":{}}\n',
  ]
  proc = MagicMock()
  proc.stdout = FakeStdout(lines)
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
async def test_codex_one_shot_text_returns_empty_when_no_agent_message(monkeypatch) -> None:
  monkeypatch.setattr(
      "src.agents.backends.codex.resolve_binary",
      lambda name, fallback: "/usr/bin/codex",
  )
  from src.agents.backends.codex import CodexBackend

  lines = [
      b'{"type":"thread.started","thread_id":"t1"}\n',
      b'{"type":"turn.completed","usage":{}}\n',
  ]
  proc = MagicMock()
  proc.stdout = FakeStdout(lines)
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
  proc.stdout = FakeStdout(lines)
  proc.stderr = MagicMock()
  proc.stderr.read = AsyncMock(return_value=b"")
  proc.wait = AsyncMock(return_value=0)
  proc.pid = 7777

  with patch("asyncio.create_subprocess_exec", new=AsyncMock(return_value=proc)) as mock_exec:
    backend = OpenCodeBackend(model=SYNTHETIC_MODEL)
    result = await backend.one_shot_text("hello prompt", "sys prompt", timeout=5.0)

  assert result == "OK title"
  args = mock_exec.await_args.args
  assert args[0] == "/usr/bin/opencode"
  assert "run" in args and "--format" in args and "json" in args
  assert args[args.index("-m") + 1] == SYNTHETIC_MODEL
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
  proc.stdout = FakeStdout(lines)
  proc.stderr = MagicMock()
  proc.stderr.read = AsyncMock(return_value=b"")
  proc.wait = AsyncMock(return_value=0)
  proc.pid = 7776

  with patch("asyncio.create_subprocess_exec", new=AsyncMock(return_value=proc)):
    backend = OpenCodeBackend(model=SYNTHETIC_MODEL)
    result = await backend.one_shot_text("hi", "sys", timeout=5.0)

  assert result == ""
