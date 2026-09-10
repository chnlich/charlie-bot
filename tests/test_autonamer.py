"""Focused regression tests for session autonaming/autogrouping."""

import json
from dataclasses import dataclass
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
from conftest import (
    ASYNCIO_CREATE_SUBPROCESS_EXEC_PATCH_TARGET,
    CODEX_RESOLVE_BINARY_PATCH_TARGET,
    OPENCODE_RESOLVE_BINARY_PATCH_TARGET,
    SYNTHETIC_MODEL,
    backend_option,
    build_light_cc_cfg,
    fake_one_shot_proc,
    make_one_shot_backend,
)

from src.core import autonamer
from src.core.autonamer import (
    iter_light_backends,
    maybe_auto_name,
    maybe_auto_name_from_claude_ai_title,
)
from src.core.config import CharlieBotConfig
from src.core.models import SessionMetadata

_BUILD_BACKEND_PATCH_TARGET = "src.core.autonamer.build_backend"
_STREAMING_BROADCAST_PATCH_TARGET = "src.core.autonamer.streaming_manager.broadcast"
_LOG_PATCH_TARGET = "src.core.autonamer.log"


@dataclass
class _AutoNameRound:
  """The mocks one maybe_auto_name round leaves behind for the test's assertions."""

  session_mgr: AsyncMock
  one_shot: AsyncMock
  build: MagicMock
  broadcast: AsyncMock
  log: MagicMock


async def _run_auto_name(
    cfg: CharlieBotConfig,
    session_id: str,
    session_name: str,
    backend: str,
    user_message: str,
    assistant_response: str,
    groups: list[str],
    *,
    one_shot: AsyncMock,
    get_session_reply: SessionMetadata | None = None,
    build_side_effect: list[object] | None = None,
) -> _AutoNameRound:
  """Run one maybe_auto_name round under the shared one-shot rig and return its mocks.

  build_backend is a MagicMock serving ``make_one_shot_backend(one_shot)`` unless *build_side_effect*
  lists the per-candidate outcomes in preference order. get_session answers every re-read with
  *get_session_reply* (None leaves the AsyncMock's auto-reply; the re-read name decides the rename,
  so the reply is per-test data). The broadcast mock is silent but captured, and the autonamer
  logger is a MagicMock the failure-path tests assert on.
  """
  meta = SessionMetadata(id=session_id, name=session_name, backend=backend)
  session_mgr = AsyncMock()
  if get_session_reply is not None:
    session_mgr.get_session.return_value = get_session_reply
  build = MagicMock(side_effect=build_side_effect) if build_side_effect is not None else MagicMock(
      return_value=make_one_shot_backend(one_shot))
  broadcast = AsyncMock()
  log_mock = MagicMock()
  with (
      patch(_BUILD_BACKEND_PATCH_TARGET, build),
      patch(_STREAMING_BROADCAST_PATCH_TARGET, new=broadcast),
      patch(_LOG_PATCH_TARGET, new=log_mock),
  ):
    await maybe_auto_name(cfg, meta, user_message, assistant_response, session_mgr, groups)
  return _AutoNameRound(session_mgr, one_shot, build, broadcast, log_mock)


def _write_claude_jsonl(home_dir: Path, session_id: str, rows: list[dict | str]) -> Path:
  jsonl_path = home_dir / ".claude" / "projects" / "encoded-cwd" / f"{session_id}.jsonl"
  jsonl_path.parent.mkdir(parents=True)
  lines = [row if isinstance(row, str) else json.dumps(row) for row in rows]
  jsonl_path.write_text("\n".join(lines) + "\n", encoding="utf-8")
  return jsonl_path


def _fallback_chain_cfg() -> CharlieBotConfig:
  """Config whose backends.preference chains a cc-claude first backend onto a codex second backend."""
  return CharlieBotConfig(
      backends={
          "options":
              [
                  backend_option(id="first-backend", label="First", type="cc-claude", model="haiku"),
                  backend_option(id="second-backend", label="Second", type="codex", model="gpt-x"),
              ],
          "preference": ["first-backend", "second-backend"],
      },)


# ---------------------------------------------------------------------------
# maybe_auto_name — light-backend one-shot path
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_maybe_auto_name_passes_existing_groups_to_backend() -> None:
  cfg = build_light_cc_cfg()
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
  cfg = build_light_cc_cfg()
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
  rig = await _run_auto_name(
      build_light_cc_cfg(),
      "session-prefix",
      "Session 42",
      "light-cc",
      "Refactor the config loader",
      "I updated the loader to use the shared config parser.",
      [],
      one_shot=AsyncMock(return_value='{"name":"Refactor Config Loader"}'),
      get_session_reply=SessionMetadata(id="session-prefix", name="Session 42"),
  )

  rig.one_shot.assert_awaited_once()
  rig.session_mgr.rename_session.assert_awaited_once_with("session-prefix", "42: Refactor Config Loader")


@pytest.mark.asyncio
async def test_maybe_auto_name_rechecks_current_name_before_renaming() -> None:
  rig = await _run_auto_name(
      build_light_cc_cfg(),
      "session-renamed",
      "Session 9",
      "light-cc",
      "Help me review the PR",
      "Here is the review summary.",
      ["Work"],
      one_shot=AsyncMock(return_value='{"name":"Generated Name","group":"Work"}'),
      get_session_reply=SessionMetadata(id="session-renamed", name="Manual Name"),
  )

  rig.session_mgr.rename_session.assert_not_awaited()
  rig.session_mgr.set_group.assert_not_awaited()
  rig.broadcast.assert_not_awaited()


_VERBATIM_NAME_ROWS = [
    pytest.param(
        "session-snake",
        7,
        '{"name":"CHARLIEBOT_HOME cleanup"}',
        "Set CHARLIEBOT_HOME for the run.",
        "Done.",
        id="snake-case-identifier",
    ),
    pytest.param(
        "session-cjk",
        8,
        '{"name":"「TRELLIS.2」分支重构"}',
        "重构「TRELLIS.2」的分支。",
        "好的，开始重构。",
        id="cjk-title",
    ),
]


@pytest.mark.asyncio
@pytest.mark.parametrize(("session_id", "session_no", "raw", "ask", "answer"), _VERBATIM_NAME_ROWS)
async def test_maybe_auto_name_keeps_backend_name_verbatim(
    session_id: str, session_no: int, raw: str, ask: str, answer: str) -> None:
  """The backend-proposed name is applied verbatim, whatever script it is written in."""
  rig = await _run_auto_name(
      build_light_cc_cfg(),
      session_id,
      f"Session {session_no}",
      "light-cc",
      ask,
      answer,
      [],
      one_shot=AsyncMock(return_value=raw),
      get_session_reply=SessionMetadata(id=session_id, name=f"Session {session_no}"),
  )

  expected_name = json.loads(raw)["name"]
  rig.session_mgr.rename_session.assert_awaited_once_with(session_id, f"{session_no}: {expected_name}")


@pytest.mark.asyncio
async def test_maybe_auto_name_falls_back_after_first_backend_failure() -> None:
  one_shot = AsyncMock(return_value='{"name":"Recovered Title","group":"Recovered"}')
  rig = await _run_auto_name(
      _fallback_chain_cfg(),
      "session-backend-failure",
      "Session 10",
      "first-backend",
      "First naming attempt",
      "Backend failed.",
      [],
      one_shot=one_shot,
      build_side_effect=[RuntimeError("unsupported reasoning effort"),
                         make_one_shot_backend(one_shot)],
      get_session_reply=SessionMetadata(id="session-backend-failure", name="Session 10", group=None),
  )

  rig.session_mgr.rename_session.assert_awaited_once_with("session-backend-failure", "10: Recovered Title")
  rig.session_mgr.set_group.assert_awaited_once_with("session-backend-failure", "Recovered")
  assert [call.args[0].id for call in rig.build.call_args_list] == ["first-backend", "second-backend"]
  one_shot.assert_awaited_once()
  rig.log.warning.assert_called_once_with(
      "autonamer_failed",
      session_id="session-backend-failure",
      error="unsupported reasoning effort",
  )
  assert rig.broadcast.await_count == 2


@pytest.mark.asyncio
async def test_maybe_auto_name_keeps_default_name_when_all_first_responses_are_unusable() -> None:
  first_one_shot = AsyncMock(return_value="")
  second_one_shot = AsyncMock(return_value='{"group":"only"}')
  rig = await _run_auto_name(
      _fallback_chain_cfg(),
      "session-exhausted",
      "Session 11",
      "first-backend",
      "Some ask",
      "Some answer.",
      [],
      one_shot=first_one_shot,
      build_side_effect=[make_one_shot_backend(first_one_shot),
                         make_one_shot_backend(second_one_shot)],
  )

  assert [call.args[0].id for call in rig.build.call_args_list] == ["first-backend", "second-backend"]
  first_one_shot.assert_awaited_once()
  second_one_shot.assert_awaited_once()
  rig.session_mgr.rename_session.assert_not_awaited()
  rig.session_mgr.set_group.assert_not_awaited()
  rig.broadcast.assert_not_awaited()
  assert rig.log.warning.call_count == 2
  assert all(call.args[0] == "autonamer_failed" for call in rig.log.warning.call_args_list)


_FALLBACK_ROWS = [
    pytest.param(
        "session-fallback",
        9,
        "Sure, here's a title: Refactoring the Loader",
        '{"name":"Loader Refactor"}',
        "Refactor the loader",
        "Done.",
        id="non-json-response",
    ),
    pytest.param(
        "session-long",
        10,
        json.dumps({"name": "x" * 61}),
        '{"name":"Short Title"}',
        "Some ask",
        "Some answer.",
        id="name-too-long",
    ),
]


@pytest.mark.asyncio
@pytest.mark.parametrize(("session_id", "session_no", "first_raw", "second_raw", "ask", "answer"), _FALLBACK_ROWS)
async def test_maybe_auto_name_falls_back_to_next_backend_on_unusable_first_name(
    session_id: str, session_no: int, first_raw: str, second_raw: str, ask: str, answer: str) -> None:
  """A first backend whose response yields no usable name falls through to the next preference."""
  first_one_shot = AsyncMock(return_value=first_raw)
  second_one_shot = AsyncMock(return_value=second_raw)
  rig = await _run_auto_name(
      _fallback_chain_cfg(),
      session_id,
      f"Session {session_no}",
      "first-backend",
      ask,
      answer,
      [],
      one_shot=first_one_shot,
      build_side_effect=[make_one_shot_backend(first_one_shot),
                         make_one_shot_backend(second_one_shot)],
      get_session_reply=SessionMetadata(id=session_id, name=f"Session {session_no}"),
  )

  assert [call.args[0].id for call in rig.build.call_args_list] == ["first-backend", "second-backend"]
  first_one_shot.assert_awaited_once()
  second_one_shot.assert_awaited_once()
  expected_name = json.loads(second_raw)["name"]
  rig.session_mgr.rename_session.assert_awaited_once_with(session_id, f"{session_no}: {expected_name}")


# ---------------------------------------------------------------------------
# iter_light_backends — ordered resolved preference iteration
# ---------------------------------------------------------------------------


def test_iter_light_backends_preserves_cross_type_preference_order() -> None:
  cfg = CharlieBotConfig(
      backends={
          "options":
              [
                  backend_option(id="claude", label="Claude", type="cc-claude", model="haiku"),
                  backend_option(id="codex", label="Codex", type="codex", model="gpt-x"),
                  backend_option(id="kimi", label="Kimi", type="cc-kimi", model="k2", credential="test-kimi"),
              ],
          "preference": ["codex", "claude", "kimi"],
      },)
  assert [option.id for option in iter_light_backends(cfg)] == ["codex", "claude", "kimi"]


def test_iter_light_backends_skips_unresolved_ids_and_duplicates() -> None:
  cfg = CharlieBotConfig(
      backends={
          "options":
              [
                  backend_option(id="claude", label="Claude", type="cc-claude", model="haiku"),
                  backend_option(id="codex", label="Codex", type="codex", model="gpt-x"),
              ],
          "preference": ["missing", "claude", "claude", "codex", "missing"],
      },)
  assert [option.id for option in iter_light_backends(cfg)] == ["claude", "codex"]


@pytest.mark.asyncio
async def test_maybe_auto_name_builds_codex_backend_for_claude_session() -> None:
  cfg = CharlieBotConfig(
      backends={
          "options":
              [
                  backend_option(id="claude-session", label="Session", type="cc-claude", model="haiku"),
                  backend_option(id="codex-gpt-5.6-luna-personal", label="Luna", type="codex", model="gpt-5.6-luna"),
              ],
          "preference": ["codex-gpt-5.6-luna-personal"],
      },)
  rig = await _run_auto_name(
      cfg,
      "session-luna",
      "Session 5",
      "claude-session",
      "Do the codex thing",
      "Done.",
      [],
      one_shot=AsyncMock(return_value='{"name":"Codex Title"}'),
      get_session_reply=SessionMetadata(id="session-luna", name="Session 5"),
  )

  built_option = rig.build.call_args.args[0]
  assert built_option.id == "codex-gpt-5.6-luna-personal"
  assert built_option.model == "gpt-5.6-luna"
  rig.one_shot.assert_awaited_once()
  rig.session_mgr.rename_session.assert_awaited_once_with("session-luna", "5: Codex Title")


@pytest.mark.asyncio
async def test_maybe_auto_name_builds_same_id_opencode_backend() -> None:
  cfg = CharlieBotConfig(
      backends={
          "options": [backend_option(id="opencode-glm52", label="OC", type="opencode", model="prov/model")],
          "preference": ["opencode-glm52"],
      },)
  rig = await _run_auto_name(
      cfg,
      "session-oc",
      "Session 6",
      "opencode-glm52",
      "Use opencode",
      "Done.",
      [],
      one_shot=AsyncMock(return_value='{"name":"Open Code Task"}'),
      get_session_reply=SessionMetadata(id="session-oc", name="Session 6"),
  )

  built_option = rig.build.call_args.args[0]
  assert built_option.id == "opencode-glm52"
  rig.one_shot.assert_awaited_once()


@pytest.mark.asyncio
async def test_maybe_auto_name_skips_loudly_when_no_preference_resolves() -> None:
  cfg = CharlieBotConfig(
      backends={
          "options": [backend_option(id="claude-session", label="Session", type="cc-claude", model="haiku"),],
          "preference": ["does-not-exist"],
      },)
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

  with patch(ASYNCIO_CREATE_SUBPROCESS_EXEC_PATCH_TARGET, new=AsyncMock(return_value=proc)) as mock_exec:
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

  with patch(ASYNCIO_CREATE_SUBPROCESS_EXEC_PATCH_TARGET, new=AsyncMock(return_value=proc)):
    backend = ClaudeCodeBackend(model="haiku")
    with pytest.raises(RuntimeError, match="claude CLI failed"):
      await backend.one_shot_text("p", "s", timeout=5.0)


@pytest.mark.asyncio
async def test_codex_one_shot_text_accumulates_agent_message(monkeypatch) -> None:
  monkeypatch.setattr(
      CODEX_RESOLVE_BINARY_PATCH_TARGET,
      lambda name, fallback: "/usr/bin/codex",
  )
  from src.agents.backends.codex import CodexBackend

  lines = [
      b'{"type":"thread.started","thread_id":"t1"}\n',
      b'{"type":"item.started","item":{"type":"agent_message","id":"a1","text":""}}\n',
      b'{"type":"item.completed","item":{"type":"agent_message","id":"a1","text":"OK title"}}\n',
      b'{"type":"turn.completed","usage":{}}\n',
  ]
  proc = fake_one_shot_proc(lines, pid=9999)

  with patch(ASYNCIO_CREATE_SUBPROCESS_EXEC_PATCH_TARGET, new=AsyncMock(return_value=proc)) as mock_exec:
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
      CODEX_RESOLVE_BINARY_PATCH_TARGET,
      lambda name, fallback: "/usr/bin/codex",
  )
  from src.agents.backends.codex import CodexBackend

  lines = [
      b'{"type":"thread.started","thread_id":"t1"}\n',
      b'{"type":"turn.completed","usage":{}}\n',
  ]
  proc = fake_one_shot_proc(lines, pid=9998)

  with patch(ASYNCIO_CREATE_SUBPROCESS_EXEC_PATCH_TARGET, new=AsyncMock(return_value=proc)):
    backend = CodexBackend(model="gpt-x")
    with pytest.raises(RuntimeError, match="no assistant text"):
      await backend.one_shot_text("hi", "sys", timeout=5.0)


@pytest.mark.asyncio
async def test_opencode_one_shot_text_extracts_text_from_flat_part_event(monkeypatch) -> None:
  """`opencode run --format json` emits flat part-shaped events
  ({"type":"text","part":{...}}), not the SSE-bus shape serve uses."""
  from src.agents.backends.opencode import OpenCodeBackend

  monkeypatch.setattr(
      OPENCODE_RESOLVE_BINARY_PATCH_TARGET,
      lambda name, fallback: "/usr/bin/opencode",
  )
  lines = [
      b'{"type":"step_start","timestamp":1,"sessionID":"s1","part":{"id":"prt_a","messageID":"m1","sessionID":"s1","type":"step-start"}}\n',
      b'{"type":"text","timestamp":2,"sessionID":"s1","part":{"id":"prt_b","messageID":"m1","sessionID":"s1","type":"text","text":"OK title","time":{"start":1,"end":2}}}\n',
      b'{"type":"step_finish","timestamp":3,"sessionID":"s1","part":{"id":"prt_c","reason":"stop","messageID":"m1","sessionID":"s1","type":"step-finish","tokens":{"total":1,"input":1,"output":1,"reasoning":0,"cache":{"write":0,"read":0}},"cost":0}}\n',
  ]
  proc = fake_one_shot_proc(lines, pid=7777)

  with patch(ASYNCIO_CREATE_SUBPROCESS_EXEC_PATCH_TARGET, new=AsyncMock(return_value=proc)) as mock_exec:
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
      OPENCODE_RESOLVE_BINARY_PATCH_TARGET,
      lambda name, fallback: "/usr/bin/opencode",
  )
  lines = [
      b'{"type":"step_start","timestamp":1,"sessionID":"s1","part":{"id":"prt_a","messageID":"m1","sessionID":"s1","type":"step-start"}}\n',
      b'{"type":"step_finish","timestamp":3,"sessionID":"s1","part":{"id":"prt_c","reason":"stop","messageID":"m1","sessionID":"s1","type":"step-finish","tokens":{"total":1,"input":1,"output":1,"reasoning":0,"cache":{"write":0,"read":0}},"cost":0}}\n',
  ]
  proc = fake_one_shot_proc(lines, pid=7776)

  with patch(ASYNCIO_CREATE_SUBPROCESS_EXEC_PATCH_TARGET, new=AsyncMock(return_value=proc)):
    backend = OpenCodeBackend(model=SYNTHETIC_MODEL)
    result = await backend.one_shot_text("hi", "sys", timeout=5.0)

  assert result == ""
