import asyncio
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


def _stub_turn_setup(monkeypatch: pytest.MonkeyPatch, jsonl: Path) -> None:
  """Stub tmux/transcript plumbing so _stream_turn drives only its poll loop."""

  async def _noop(*args, **kwargs) -> None:
    return None

  monkeypatch.setattr(claude_sub, "ensure_tmux_session", _noop)
  monkeypatch.setattr(claude_sub, "_wait_for_prompt", _noop)
  monkeypatch.setattr(claude_sub, "_send_prompt", _noop)
  monkeypatch.setattr(claude_sub, "_find_existing_claude_jsonl", lambda session_id: jsonl)


def test_parse_argv_accepts_cc_backend_shape_with_multiline_prompt() -> None:
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
          "--",
          "hello\nworld",
      ])

  assert args.output_format == "stream-json"
  assert args.prompt == "hello\nworld"
  assert args.model == "claude-opus-4-8"
  assert args.effort == "max"
  assert args.resume == "session-id"
  assert args.disallowed_tools == ["Monitor,CronCreate"]


def test_parse_argv_rejects_non_stream_json_output() -> None:
  with pytest.raises(ValueError, match="stream-json"):
    claude_sub.parse_argv(["-p", "--output-format", "json", "--", "hello"])


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
          "--",
          "hi",
      ])

  assert args.disallowed_tools == ["Monitor,CronCreate", "AskUserQuestion,ExitPlanMode"]


def test_pane_has_interactive_menu_detects_numbered_selection() -> None:
  plan_menu = (
      "Would you like to proceed?\n"
      "❯ 1. Yes, and auto-accept edits\n"
      "  2. Yes, and manually approve edits\n"
      "  3. No, keep planning\n")
  permission_menu = "│ ❯ 1. Yes                                  │"

  assert claude_sub._pane_has_interactive_menu(plan_menu)
  assert claude_sub._pane_has_interactive_menu(permission_menu)


def test_pane_has_interactive_menu_ignores_prompt_and_output() -> None:
  idle_prompt = "❯ Try \"edit a file\""
  working = "✶ Running… (esc to interrupt)\n❯ "
  numbered_text = "Plan:\n1. first step\n2. second step"

  assert not claude_sub._pane_has_interactive_menu(idle_prompt)
  assert not claude_sub._pane_has_interactive_menu(working)
  assert not claude_sub._pane_has_interactive_menu(numbered_text)


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
