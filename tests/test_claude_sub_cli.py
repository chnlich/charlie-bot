import asyncio
import json
from pathlib import Path

import pytest

from src.cli import claude_sub


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


def test_pane_shows_unsubmitted_prompt_detects_pasted_text() -> None:
  prompt = "Fix the recurring prompt-submit race in src/cli/claude_sub.py.\nDetails follow on later lines."
  pane = ("some earlier scrollback\n"
          "❯ Fix the recurring prompt-submit race in src/cli/claude_sub.py. Details\n"
          "  follow on later lines.\n")

  assert claude_sub._pane_shows_unsubmitted_prompt(pane, prompt)


def test_pane_shows_unsubmitted_prompt_ignores_idle_empty_prompt() -> None:
  idle = "✶ Welcome\n❯ \n"
  placeholder = "❯ Try \"edit a file\"\n"

  assert not claude_sub._pane_shows_unsubmitted_prompt(idle, "Fix the race")
  assert not claude_sub._pane_shows_unsubmitted_prompt(placeholder, "Fix the race")


def test_pane_shows_unsubmitted_prompt_ignores_busy_turn_pane() -> None:
  pane = ("> Fix the race in claude_sub\n"
          "✶ Running… (esc to interrupt)\n"
          "❯ \n")

  assert not claude_sub._pane_shows_unsubmitted_prompt(pane, "Fix the race in claude_sub")


@pytest.mark.asyncio
async def test_send_prompt_resends_enter_until_submitted(monkeypatch: pytest.MonkeyPatch) -> None:
  enters = 0

  async def fake_run_tmux_bytes(*args, stdin=None, capture=False) -> bytes:
    nonlocal enters
    if args[0] == "send-keys":
      enters += 1
    return b""

  panes = iter([
      "❯ Fix the race\n",  # first Enter dropped: prompt still in the input box
      "> Fix the race\n✶ Running…\n❯ \n",  # second Enter landed
  ])

  async def fake_capture(session_id: str) -> str:
    return next(panes)

  monkeypatch.setattr(claude_sub, "_run_tmux_bytes", fake_run_tmux_bytes)
  monkeypatch.setattr(claude_sub, "_capture_pane", fake_capture)
  monkeypatch.setattr(claude_sub, "_SUBMIT_VERIFY_WAIT_SECONDS", 0.0)
  monkeypatch.setattr(claude_sub, "_SUBMIT_RETRY_WAIT_SECONDS", 0.0)

  await claude_sub._send_prompt("session-id", "Fix the race")

  assert enters == 2


@pytest.mark.asyncio
async def test_send_prompt_raises_when_enter_never_lands(monkeypatch: pytest.MonkeyPatch) -> None:
  enters = 0

  async def fake_run_tmux_bytes(*args, stdin=None, capture=False) -> bytes:
    nonlocal enters
    if args[0] == "send-keys":
      enters += 1
    return b""

  async def fake_capture(session_id: str) -> str:
    return "❯ Fix the race\n"

  monkeypatch.setattr(claude_sub, "_run_tmux_bytes", fake_run_tmux_bytes)
  monkeypatch.setattr(claude_sub, "_capture_pane", fake_capture)
  monkeypatch.setattr(claude_sub, "_SUBMIT_VERIFY_WAIT_SECONDS", 0.0)
  monkeypatch.setattr(claude_sub, "_SUBMIT_RETRY_WAIT_SECONDS", 0.0)

  with pytest.raises(RuntimeError, match="unsubmitted"):
    await claude_sub._send_prompt("session-id", "Fix the race")

  assert enters == claude_sub._SUBMIT_MAX_ENTER_ATTEMPTS


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
  monkeypatch.setattr(claude_sub, "_capture_pane", fake_capture)
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

  monkeypatch.setattr(claude_sub, "_capture_pane", fake_capture)
  monkeypatch.setattr(claude_sub, "_interrupt_turn", fake_interrupt)

  args = claude_sub.ClaudeSubArgs(output_format="stream-json", prompt="hi")
  await claude_sub._stream_turn(args, asyncio.Event())  # returns normally

  assert not capture_called  # completed before any stall probe
  assert not interrupt_called
