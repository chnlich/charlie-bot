import asyncio
import io
import json
from pathlib import Path

import pytest

from src.cli import claude_sub

# Real `tmux capture-pane -e` fixtures from claude v2.1.174. Ghost suggestions and
# idle hints render SGR dim (\x1b[2m) and must read as EMPTY real content; pasted
# text (placeholder or literal) renders non-dim and must read as NON-empty.
_FIXTURE_IDLE_HINT = '\x1b[39m❯ \x1b[2mTry "how do I log an error?"\x1b[0m'
_FIXTURE_GHOST_SUGGESTION = "\x1b[39m❯ \x1b[2m3D latent 我打算用 sparse voxel，变长的\x1b[0m"
_FIXTURE_PASTE_PLACEHOLDER = "\x1b[39m❯ [Pasted text #1 +5 lines]"
_FIXTURE_LITERAL_PASTE = "\x1b[39m❯ short literal paste"
_FIXTURE_STARTUP_MENU = """\
\x1b[?25l╭────────────────────────────────────────────╮
│ Claude Code can help with this repository.   │
│ ❯ 1. Yes, try it                             │
│   2. No, keep using the terminal             │
╰────────────────────────────────────────────╯"""


def _stub_turn_setup(monkeypatch: pytest.MonkeyPatch, jsonl: Path) -> None:
  """Stub tmux/transcript plumbing so _stream_turn drives only its poll loop."""

  async def _noop(*args, **kwargs) -> None:
    return None

  monkeypatch.setattr(claude_sub, "ensure_tmux_session", _noop)
  monkeypatch.setattr(claude_sub, "_wait_for_prompt", _noop)
  monkeypatch.setattr(claude_sub, "_send_prompt", _noop)
  monkeypatch.setattr(claude_sub, "_find_existing_claude_jsonl", lambda session_id: jsonl)


def test_parse_argv_accepts_cc_backend_flags_without_prompt() -> None:
  args = claude_sub.parse_argv(
      [
          "-p",
          "--output-format",
          "stream-json",
          "--verbose",
          "--dangerously-skip-permissions",
          "--disallowed-tools",
          "Monitor,CronCreate",
          "--model",
          "claude-opus-4-8",
          "--effort",
          "max",
          "--resume",
          "session-id",
      ])

  assert args.output_format == "stream-json"
  assert args.prompt == ""
  assert args.model == "claude-opus-4-8"
  assert args.effort == "max"
  assert args.resume == "session-id"
  assert args.disallowed_tools == ["Monitor,CronCreate"]


def test_parse_argv_rejects_non_stream_json_output() -> None:
  with pytest.raises(ValueError, match="stream-json"):
    claude_sub.parse_argv(["-p", "--output-format", "json"])


def test_parse_argv_rejects_positional_prompt_tokens() -> None:
  with pytest.raises(ValueError, match="stdin"):
    claude_sub.parse_argv(["-p", "--output-format", "stream-json", "--", "hello"])


def test_parse_argv_collects_multiple_disallowed_tools_flags() -> None:
  args = claude_sub.parse_argv(
      [
          "-p",
          "--output-format",
          "stream-json",
          "--disallowed-tools",
          "Monitor,CronCreate",
          "--disallowed-tools",
          "AskUserQuestion,ExitPlanMode",
      ])

  assert args.disallowed_tools == ["Monitor,CronCreate", "AskUserQuestion,ExitPlanMode"]


def test_main_reads_prompt_from_stdin(monkeypatch: pytest.MonkeyPatch) -> None:
  captured: dict[str, claude_sub.ClaudeSubArgs] = {}

  async def fake_run(args: claude_sub.ClaudeSubArgs) -> None:
    captured["args"] = args

  monkeypatch.setattr(claude_sub, "_run", fake_run)
  monkeypatch.setattr(claude_sub.sys, "stdin", io.StringIO("hello\nworld"))

  assert claude_sub.main(["-p", "--output-format", "stream-json"]) == 0
  assert captured["args"].prompt == "hello\nworld"


def test_pane_has_interactive_menu_detects_numbered_selection() -> None:
  plan_menu = (
      "Would you like to proceed?\n"
      "❯ 1. Yes, and auto-accept edits\n"
      "  2. Yes, and manually approve edits\n"
      "  3. No, keep planning\n")
  permission_menu = "│ ❯ 1. Yes                                  │"

  assert claude_sub._pane_has_interactive_menu(plan_menu)
  assert claude_sub._pane_has_interactive_menu(permission_menu)


def test_classify_pane_input_detects_fullscreen_startup_menu() -> None:
  state = claude_sub._classify_pane_input(_FIXTURE_STARTUP_MENU)

  assert state.kind == claude_sub.PaneInputKind.MENU
  assert "Yes, try it" in state.content


def test_classify_pane_input_detects_empty_prompt() -> None:
  state = claude_sub._classify_pane_input(_FIXTURE_IDLE_HINT)

  assert state == claude_sub.PaneInputState(claude_sub.PaneInputKind.PROMPT, "")


def test_classify_pane_input_detects_multiline_paste_placeholder() -> None:
  state = claude_sub._classify_pane_input(_FIXTURE_PASTE_PLACEHOLDER)

  assert state == claude_sub.PaneInputState(
      claude_sub.PaneInputKind.PROMPT, "[Pasted text #1 +5 lines]")


def test_classify_pane_input_uses_last_cursor_line_over_scrollback_echo() -> None:
  pane = _FIXTURE_STARTUP_MENU + "\n" + _FIXTURE_IDLE_HINT

  state = claude_sub._classify_pane_input(pane)

  assert state == claude_sub.PaneInputState(claude_sub.PaneInputKind.PROMPT, "")


def test_pane_has_interactive_menu_ignores_prompt_and_output() -> None:
  idle_prompt = "❯ Try \"edit a file\""
  working = "✶ Running… (esc to interrupt)\n❯ "
  numbered_text = "Plan:\n1. first step\n2. second step"
  numbered_prompt = "1. bf16 dtype-aware parity\n2. capture the exact failure"
  numbered_prompt_echo_only = "❯ 1. bf16 dtype-aware parity\n  2. capture the exact failure"
  numbered_prompt_echo = (
      "❯ 1. Inspect the current failure\n"
      "  2. Patch the narrowest fix\n"
      "✶ Running… (esc to interrupt)\n" + _FIXTURE_IDLE_HINT)

  assert not claude_sub._pane_has_interactive_menu(idle_prompt)
  assert not claude_sub._pane_has_interactive_menu(working)
  assert not claude_sub._pane_has_interactive_menu(numbered_text)
  assert not claude_sub._pane_has_interactive_menu(numbered_prompt_echo_only, numbered_prompt)
  assert not claude_sub._pane_has_interactive_menu(numbered_prompt_echo)


@pytest.mark.asyncio
async def test_run_tmux_bytes_strips_session_env(monkeypatch: pytest.MonkeyPatch) -> None:
  captured: dict[str, dict[str, str]] = {}
  monkeypatch.setenv("CHARLIEBOT_SESSION_ID", "stale-session")
  monkeypatch.setattr(claude_sub, "_tmux_binary", lambda: "/usr/bin/tmux")

  class FakeProcess:
    returncode = 0

    async def communicate(self, stdin):
      return b"pane", b""

  async def fake_create_subprocess_exec(*args, **kwargs):
    captured["env"] = kwargs["env"]
    return FakeProcess()

  monkeypatch.setattr(claude_sub.asyncio, "create_subprocess_exec", fake_create_subprocess_exec)

  output = await claude_sub._run_tmux_bytes("capture-pane", "-p", capture=True)

  assert output == b"pane"
  assert "CHARLIEBOT_SESSION_ID" not in captured["env"]


def test_pane_has_interactive_menu_detects_current_menu_after_numbered_prompt_echo() -> None:
  pane = (
      "❯ 1. Inspect the current failure\n"
      "  2. Patch the narrowest fix\n"
      "Would you like to proceed?\n"
      "❯ 1. Yes, and auto-accept edits\n"
      "  2. No, keep planning\n")

  assert claude_sub._pane_has_interactive_menu(
      pane, "1. Inspect the current failure\n2. Patch the narrowest fix")


def test_pane_has_interactive_menu_ignores_digit_leading_dim_ghost_suggestion() -> None:
  # A dim ghost suggestion starting with a digit must not read as a blocking menu:
  # in a plain capture this line is "❯ 3D latent …" and would match _MENU_OPTION_RE,
  # aborting a healthy long turn. Dim-stripping leaves only the bare "❯ ".
  plain_equivalent = "❯ 3D latent 我打算用 sparse voxel，变长的"

  assert claude_sub._pane_has_interactive_menu(plain_equivalent)  # plain capture is unsound
  assert not claude_sub._pane_has_interactive_menu(_FIXTURE_GHOST_SUGGESTION)


def test_strip_dim_text_ignores_extended_color_arguments() -> None:
  # Live panes emit extended-color SGR (e.g. \x1b[48;5;22m diff backgrounds): the color
  # arguments are not attributes, so the 22 in 48;5;22 must not read as SGR 22 (dim off),
  # nor the 2 in a 38;5;2 foreground as SGR 2 (dim on).
  assert claude_sub._strip_dim_text("\x1b[38;5;2m❯ real text\x1b[0m") == "❯ real text"
  assert claude_sub._strip_dim_text("\x1b[2mghost\x1b[48;5;22m still ghost\x1b[0m real") == " real"


def test_input_box_content_reads_dim_hint_and_ghost_as_empty() -> None:
  assert claude_sub._input_box_content(_FIXTURE_IDLE_HINT) == ""
  assert claude_sub._input_box_content(_FIXTURE_GHOST_SUGGESTION) == ""


def test_input_box_content_reads_pasted_text_as_nonempty() -> None:
  assert claude_sub._input_box_content(_FIXTURE_PASTE_PLACEHOLDER) == "[Pasted text #1 +5 lines]"
  assert claude_sub._input_box_content(_FIXTURE_LITERAL_PASTE) == "short literal paste"


def test_input_box_content_uses_last_prompt_line_so_scrollback_echo_reads_submitted() -> None:
  pane = ("❯ Fix the recurring prompt-submit race in src/cli/claude_sub.py. Details\n"
          "  follow on later lines.\n"
          "✶ Running… (esc to interrupt)\n" + _FIXTURE_IDLE_HINT + "\n")

  assert claude_sub._input_box_content(pane) == ""


def test_input_box_content_returns_none_without_prompt_line() -> None:
  assert claude_sub._input_box_content("✶ Running… (esc to interrupt)\n") is None


def test_pane_has_prompt_detects_tall_composer_input_box() -> None:
  pane = """─────────────────────────────────────────
❯ [Artifact comments · /files/.../plan_comment_fix.html] (4)
  1. ▸ 2 Fix — web/static/js/artifact-comments.js › "2
     Fix —
  2. ▸ 2 Fix — ... › "Use cleanText(block, 400) ..."
     ↳ B
  3. ▸ 2 Fix — ... › "buildBatchMessage numbers the first line ..."
     ↳ C
  4. ▸ In scope › "Robust multi-line handling in the message builder."
     ↳ D
─────────────────────────────────────────
  ⏵⏵ bypass permissions on (shift+tab to cycle)
"""

  assert claude_sub._pane_has_prompt(pane)


def test_pane_has_prompt_returns_false_without_input_box() -> None:
  assert not claude_sub._pane_has_prompt("✶ Booting Claude…\n  Loading session\n")


def _stub_send_prompt_tmux(monkeypatch: pytest.MonkeyPatch, captures) -> list[str]:
  """Stub tmux for _send_prompt tests: zero waits, scripted -e pane captures.

  Returns the list of send-keys keystrokes recorded during the call.
  """
  keys: list[str] = []

  async def fake_run_tmux_bytes(*args, stdin=None, capture=False) -> bytes:
    if args[0] == "send-keys":
      keys.append(args[-1])
    return b""

  pane_iter = iter(captures)

  async def fake_capture_escapes(session_id: str) -> str:
    return next(pane_iter)

  monkeypatch.setattr(claude_sub, "_run_tmux_bytes", fake_run_tmux_bytes)
  monkeypatch.setattr(claude_sub, "_capture_pane_escapes", fake_capture_escapes)
  monkeypatch.setattr(claude_sub, "_SUBMIT_VERIFY_WAIT_SECONDS", 0.0)
  monkeypatch.setattr(claude_sub, "_SUBMIT_RETRY_WAIT_SECONDS", 0.0)
  monkeypatch.setattr(claude_sub, "_PASTE_RENDER_TIMEOUT_SECONDS", 0.0)
  monkeypatch.setattr(claude_sub, "_CLEAR_INPUT_TIMEOUT_SECONDS", 0.0)
  monkeypatch.setattr(claude_sub, "_POLL_SECONDS", 0.0)
  return keys


def _forever(first: list[str], repeated: str):
  yield from first
  while True:
    yield repeated


@pytest.mark.asyncio
async def test_send_prompt_resends_enter_until_input_box_empties(monkeypatch: pytest.MonkeyPatch) -> None:
  keys = _stub_send_prompt_tmux(
      monkeypatch,
      [
          _FIXTURE_IDLE_HINT,  # pre-paste: only a dim hint -> empty, nothing to clear
          _FIXTURE_PASTE_PLACEHOLDER,  # paste rendered as the multi-line placeholder
          _FIXTURE_PASTE_PLACEHOLDER,  # first Enter dropped: box still non-empty
          _FIXTURE_IDLE_HINT,  # second Enter landed: box empty again (ghost hint only)
      ])

  await claude_sub._send_prompt("session-id", "Fix the race\nwith details on later lines")

  assert keys == ["Enter", "Enter"]


@pytest.mark.asyncio
async def test_send_prompt_does_not_retry_when_ghost_text_fills_empty_box(monkeypatch: pytest.MonkeyPatch) -> None:
  keys = _stub_send_prompt_tmux(
      monkeypatch,
      [
          _FIXTURE_IDLE_HINT,  # pre-paste: empty
          _FIXTURE_LITERAL_PASTE,  # paste rendered literally
          _FIXTURE_GHOST_SUGGESTION,  # Enter landed; a dim ghost suggestion reappeared in the emptied box
      ])

  await claude_sub._send_prompt("session-id", "short literal paste")

  assert keys == ["Enter"]  # ghost text must not look unsubmitted and trigger stray Enters


@pytest.mark.asyncio
async def test_send_prompt_raises_with_box_content_when_enter_never_lands(monkeypatch: pytest.MonkeyPatch) -> None:
  keys = _stub_send_prompt_tmux(monkeypatch, _forever([_FIXTURE_IDLE_HINT], _FIXTURE_PASTE_PLACEHOLDER))

  with pytest.raises(RuntimeError, match=r"unsubmitted.*\[Pasted text #1 \+5 lines\]"):
    await claude_sub._send_prompt("session-id", "Fix the race\nwith details on later lines")

  assert keys == ["Enter"] * claude_sub._SUBMIT_MAX_ENTER_ATTEMPTS


@pytest.mark.asyncio
async def test_send_prompt_clears_stale_input_before_pasting(monkeypatch: pytest.MonkeyPatch) -> None:
  keys = _stub_send_prompt_tmux(
      monkeypatch,
      [
          _FIXTURE_LITERAL_PASTE,  # stale text from a prior failed turn
          _FIXTURE_IDLE_HINT,  # C-e/C-u cleared it
          _FIXTURE_PASTE_PLACEHOLDER,  # paste rendered
          _FIXTURE_IDLE_HINT,  # Enter landed
      ])

  await claude_sub._send_prompt("session-id", "Fix the race\nwith details on later lines")

  assert keys == ["C-e", "C-u", "Enter"]


@pytest.mark.asyncio
async def test_send_prompt_dismisses_startup_menu_before_pasting(monkeypatch: pytest.MonkeyPatch) -> None:
  events: list[dict] = []
  keys = _stub_send_prompt_tmux(
      monkeypatch,
      [
          _FIXTURE_STARTUP_MENU,  # pre-send check sees a startup menu, not stale input
          _FIXTURE_STARTUP_MENU,  # bounded cancel path sends Escape
          _FIXTURE_IDLE_HINT,  # menu dismissed; real prompt is ready
          _FIXTURE_PASTE_PLACEHOLDER,  # paste rendered
          _FIXTURE_IDLE_HINT,  # Enter landed
      ])

  await claude_sub._send_prompt("session-id", "Fix the race\nwith details on later lines", events.append)

  assert keys == ["Escape", "Enter"]
  assert [event["subtype"] for event in events] == ["tui_menu_dismissed"]


@pytest.mark.asyncio
async def test_send_prompt_raises_when_stale_input_never_clears(monkeypatch: pytest.MonkeyPatch) -> None:
  keys = _stub_send_prompt_tmux(monkeypatch, _forever([], _FIXTURE_LITERAL_PASTE))

  with pytest.raises(RuntimeError, match="stale content.*short literal paste"):
    await claude_sub._send_prompt("session-id", "Fix the race")

  assert keys == ["C-e", "C-u"]  # never pasted, never sent Enter


@pytest.mark.asyncio
async def test_send_prompt_raises_when_paste_never_renders(monkeypatch: pytest.MonkeyPatch) -> None:
  keys = _stub_send_prompt_tmux(monkeypatch, _forever([], _FIXTURE_IDLE_HINT))

  with pytest.raises(RuntimeError, match="did not render"):
    await claude_sub._send_prompt("session-id", "Fix the race")

  assert keys == []  # no Enter ever sent toward an empty box


@pytest.mark.asyncio
async def test_wait_for_prompt_dismisses_startup_menu_and_emits_warning(monkeypatch: pytest.MonkeyPatch) -> None:
  captures = iter([_FIXTURE_STARTUP_MENU, _FIXTURE_IDLE_HINT])
  keys: list[str] = []
  events: list[dict] = []

  async def fake_capture(session_id: str) -> str:
    return next(captures)

  async def fake_run_tmux_bytes(*args, stdin=None, capture=False) -> bytes:
    if args[0] == "send-keys":
      keys.append(args[-1])
    return b""

  monkeypatch.setattr(claude_sub, "_capture_pane_escapes", fake_capture)
  monkeypatch.setattr(claude_sub, "_run_tmux_bytes", fake_run_tmux_bytes)
  monkeypatch.setattr(claude_sub, "_POLL_SECONDS", 0.0)

  await claude_sub._wait_for_prompt("session-id", events.append)

  assert keys == ["Escape"]
  assert len(events) == 1
  assert events[0]["type"] == "system"
  assert events[0]["subtype"] == "tui_menu_dismissed"
  assert events[0]["content"] == claude_sub._TUI_MENU_DISMISSED_WARNING
  assert events[0]["uuid"]


@pytest.mark.asyncio
async def test_wait_for_prompt_fails_loud_after_startup_menu_dismiss_bound(monkeypatch: pytest.MonkeyPatch) -> None:
  keys: list[str] = []

  async def fake_capture(session_id: str) -> str:
    return _FIXTURE_STARTUP_MENU

  async def fake_run_tmux_bytes(*args, stdin=None, capture=False) -> bytes:
    if args[0] == "send-keys":
      keys.append(args[-1])
    return b""

  monkeypatch.setattr(claude_sub, "_capture_pane_escapes", fake_capture)
  monkeypatch.setattr(claude_sub, "_run_tmux_bytes", fake_run_tmux_bytes)
  monkeypatch.setattr(claude_sub, "_PRE_PROMPT_MENU_MAX_DISMISS", 2)
  monkeypatch.setattr(claude_sub, "_POLL_SECONDS", 0.0)

  with pytest.raises(RuntimeError, match="startup menu.*before the first prompt") as exc_info:
    await claude_sub._wait_for_prompt("session-id", lambda event: None)

  assert keys == ["Escape", "Escape"]
  assert "120" not in str(exc_info.value)


def test_convert_record_suppresses_typed_user_prompt() -> None:
  event = claude_sub._convert_record(
      {
          "type": "user",
          "message": {
              "role": "user",
              "content": "hello",
          },
          "sessionId": "session-id",
      },
      "session-id",
  )

  assert event is None


def test_convert_record_preserves_tool_result_user_event() -> None:
  event = claude_sub._convert_record(
      {
          "type": "user",
          "message":
              {
                  "role": "user",
                  "content": [{
                      "type": "tool_result",
                      "tool_use_id": "toolu_123",
                      "content": "ok",
                  }],
              },
          "toolUseResult": {
              "stdout": "ok"
          },
          "sessionId": "session-id",
          "uuid": "event-id",
      },
      "session-id",
  )

  assert event == {
      "type": "user",
      "message": {
          "role": "user",
          "content": [{
              "type": "tool_result",
              "tool_use_id": "toolu_123",
              "content": "ok",
          }],
      },
      "parent_tool_use_id": None,
      "session_id": "session-id",
      "uuid": "event-id",
      "tool_use_result": {
          "stdout": "ok"
      },
  }


def test_result_event_uses_aggregate_usage_from_unique_messages() -> None:
  state = claude_sub.TurnState()
  state.observe_assistant(
      {
          "id": "msg-a",
          "content": [{
              "type": "text",
              "text": "final",
          }],
          "stop_reason": "end_turn",
          "usage":
              {
                  "input_tokens": 10,
                  "output_tokens": 2,
                  "cache_read_input_tokens": 3,
                  "cache_creation_input_tokens": 4,
              },
      })

  event = claude_sub._result_event("session-id", state, {"durationMs": 123})

  assert event["type"] == "result"
  assert event["subtype"] == "success"
  assert event["session_id"] == "session-id"
  assert event["result"] == "final"
  assert event["usage"]["input_tokens"] == 10
  assert event["usage"]["output_tokens"] == 2


@pytest.mark.asyncio
async def test_resume_requires_existing_transcript_or_tmux_session(monkeypatch: pytest.MonkeyPatch) -> None:
  ensure_called = False

  async def fake_tmux_session_exists(session_id: str) -> bool:
    return False

  async def fake_ensure_tmux_session(*args, **kwargs) -> None:
    nonlocal ensure_called
    ensure_called = True

  monkeypatch.setattr(claude_sub, "_find_existing_claude_jsonl", lambda session_id: None)
  monkeypatch.setattr(claude_sub, "tmux_session_exists", fake_tmux_session_exists)
  monkeypatch.setattr(claude_sub, "ensure_tmux_session", fake_ensure_tmux_session)

  args = claude_sub.ClaudeSubArgs(
      output_format="stream-json",
      prompt="hello",
      resume="missing-session",
  )
  with pytest.raises(RuntimeError, match="resume session not found: missing-session"):
    await claude_sub._stream_turn(args, asyncio.Event())

  assert not ensure_called


@pytest.mark.asyncio
async def test_stream_turn_emits_init_before_prompt_wait_warning(monkeypatch: pytest.MonkeyPatch, tmp_path: Path,
                                                                 capsys: pytest.CaptureFixture[str]) -> None:
  jsonl = tmp_path / "session.jsonl"
  records = [
      {
          "type": "assistant",
          "message": {
              "id": "msg-a",
              "content": [{
                  "type": "text",
                  "text": "done",
              }],
          },
      },
      {
          "type": "system",
          "subtype": "turn_duration",
          "durationMs": 5,
      },
  ]
  jsonl.write_text("".join(json.dumps(r) + "\n" for r in records), encoding="utf-8")

  async def fake_ensure_tmux_session(*args, **kwargs) -> None:
    print("tmux startup noise")

  async def fake_wait_for_prompt(session_id: str, emit) -> None:
    emit({
        "type": "system",
        "subtype": "tui_menu_dismissed",
        "content": claude_sub._TUI_MENU_DISMISSED_WARNING,
        "uuid": "warning-id",
    })

  async def fake_send_prompt(session_id: str, prompt: str, emit=None) -> None:
    return None

  async def fake_wait_transcript(session_id: str) -> Path:
    return jsonl

  monkeypatch.setattr(claude_sub, "ensure_tmux_session", fake_ensure_tmux_session)
  monkeypatch.setattr(claude_sub, "_wait_for_prompt", fake_wait_for_prompt)
  monkeypatch.setattr(claude_sub, "_send_prompt", fake_send_prompt)
  monkeypatch.setattr(claude_sub, "_wait_for_transcript_path", fake_wait_transcript)
  monkeypatch.setattr(claude_sub, "_find_existing_claude_jsonl", lambda session_id: None)

  args = claude_sub.ClaudeSubArgs(output_format="stream-json", prompt="hi")
  await claude_sub._stream_turn(args, asyncio.Event())

  output = capsys.readouterr().out
  assert "tmux startup noise" not in output
  events = [json.loads(line) for line in output.splitlines()]
  assert events[0]["type"] == "system"
  assert events[0]["subtype"] == "init"
  assert events[1] == {
      "type": "system",
      "subtype": "tui_menu_dismissed",
      "content": claude_sub._TUI_MENU_DISMISSED_WARNING,
      "uuid": "warning-id",
  }


@pytest.mark.asyncio
async def test_stream_turn_aborts_on_blocking_menu(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  jsonl = tmp_path / "session.jsonl"
  jsonl.write_text("", encoding="utf-8")  # no records ever -> transcript stays quiet
  _stub_turn_setup(monkeypatch, jsonl)

  interrupted = False

  async def fake_interrupt(session_id: str) -> None:
    nonlocal interrupted
    interrupted = True

  async def fake_capture(session_id: str) -> str:
    return "Would you like to proceed?\n❯ 1. Yes\n  2. No"

  monkeypatch.setattr(claude_sub, "_interrupt_turn", fake_interrupt)
  monkeypatch.setattr(claude_sub, "_capture_pane_escapes", fake_capture)
  monkeypatch.setattr(claude_sub, "_STALL_PROBE_SECONDS", 0.0)
  monkeypatch.setattr(claude_sub, "_PANE_PROBE_INTERVAL_SECONDS", 0.0)
  monkeypatch.setattr(claude_sub, "_POLL_SECONDS", 0.0)

  args = claude_sub.ClaudeSubArgs(output_format="stream-json", prompt="hi")
  with pytest.raises(RuntimeError, match="interactive TUI menu"):
    await claude_sub._stream_turn(args, asyncio.Event())

  assert interrupted  # the TUI was C-c'd before raising


@pytest.mark.asyncio
async def test_stream_turn_does_not_abort_on_numbered_prompt_echo(monkeypatch: pytest.MonkeyPatch,
                                                                  tmp_path: Path) -> None:
  jsonl = tmp_path / "session.jsonl"
  jsonl.write_text("", encoding="utf-8")  # no records ever -> transcript stays quiet
  _stub_turn_setup(monkeypatch, jsonl)

  capture_called = False

  async def fake_interrupt(session_id: str) -> None:
    return None

  async def fake_capture(session_id: str) -> str:
    nonlocal capture_called
    capture_called = True
    return "❯ 1. Inspect the current failure\n  2. Patch the narrowest fix"

  monkeypatch.setattr(claude_sub, "_interrupt_turn", fake_interrupt)
  monkeypatch.setattr(claude_sub, "_capture_pane_escapes", fake_capture)
  monkeypatch.setattr(claude_sub, "_STALL_PROBE_SECONDS", 0.0)
  monkeypatch.setattr(claude_sub, "_PANE_PROBE_INTERVAL_SECONDS", 0.0)
  monkeypatch.setattr(claude_sub, "_MENU_CONFIRM_PROBES", 1)
  monkeypatch.setattr(claude_sub, "_TURN_HARD_CAP_SECONDS", 0.0)

  args = claude_sub.ClaudeSubArgs(
      output_format="stream-json",
      prompt="1. Inspect the current failure\n2. Patch the narrowest fix",
  )
  with pytest.raises(RuntimeError, match="turn exceeded"):
    await claude_sub._stream_turn(args, asyncio.Event())

  assert capture_called


@pytest.mark.asyncio
async def test_stream_turn_completes_without_aborting(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  jsonl = tmp_path / "session.jsonl"
  records = [
      {
          "type": "assistant",
          "message": {
              "id": "msg-a",
              "content": [{
                  "type": "text",
                  "text": "done",
              }],
          },
      },
      {
          "type": "system",
          "subtype": "turn_duration",
          "durationMs": 5,
      },
  ]
  jsonl.write_text("".join(json.dumps(r) + "\n" for r in records), encoding="utf-8")

  async def fake_wait_transcript(session_id: str) -> Path:
    return jsonl

  _stub_turn_setup(monkeypatch, jsonl)
  monkeypatch.setattr(claude_sub, "_wait_for_transcript_path", fake_wait_transcript)
  # offset must start at 0 so the tail reads the pre-written records.
  monkeypatch.setattr(claude_sub, "_find_existing_claude_jsonl", lambda session_id: None)

  capture_called = False
  interrupt_called = False

  async def fake_capture(session_id: str) -> str:
    nonlocal capture_called
    capture_called = True
    return ""

  async def fake_interrupt(session_id: str) -> None:
    nonlocal interrupt_called
    interrupt_called = True

  monkeypatch.setattr(claude_sub, "_capture_pane_escapes", fake_capture)
  monkeypatch.setattr(claude_sub, "_interrupt_turn", fake_interrupt)

  args = claude_sub.ClaudeSubArgs(output_format="stream-json", prompt="hi")
  await claude_sub._stream_turn(args, asyncio.Event())  # returns normally

  assert not capture_called  # completed before any stall probe
  assert not interrupt_called
