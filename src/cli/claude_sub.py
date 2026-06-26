"""Subscription-backed drop-in for `claude -p --output-format stream-json`.

The command accepts the flags emitted by ClaudeCodeBackend, reads the prompt
from stdin, drives an interactive Claude TUI in tmux, and mirrors the TUI
transcript JSONL back as Claude Code stream-json events.
"""

from __future__ import annotations

import asyncio
import contextlib
import io
import json
import signal
import sys
import time
import uuid
from dataclasses import dataclass, field, replace
from pathlib import Path
from typing import Any, Optional

from src.agents.backends.pty_common import _TMUX_SOCKET, _tmux_binary, _tmux_client_env, tmux_session_name
from src.agents.backends.tui import _find_existing_claude_jsonl, ensure_tmux_session, tmux_session_exists
from src.cli import claude_tui_state
from src.cli.claude_tui_state import PaneInputKind
from src.cli.claude_tui_state import PaneInputState

_POLL_SECONDS = 0.1
_PROMPT_READY_TIMEOUT_SECONDS = 60.0
_TRANSCRIPT_PATH_TIMEOUT_SECONDS = 60.0
_BRACKETED_PASTE_START = b"\x1b[200~"
_BRACKETED_PASTE_END = b"\x1b[201~"

# Turn-level stall guard. The TUI writes no transcript records while it does long
# autonomous work (tool execution, extended thinking) *and* while it sits blocked on
# an interactive menu it cannot answer. Only the latter must abort, so once the
# transcript has been quiet for _STALL_PROBE_SECONDS we inspect the pane for a
# blocking menu rather than killing on inactivity alone. _TURN_HARD_CAP_SECONDS is a
# generous last-resort backstop for any block that does not render as such a menu.
_STALL_PROBE_SECONDS = 30.0
_PANE_PROBE_INTERVAL_SECONDS = 5.0
_MENU_CONFIRM_PROBES = 2
_TURN_HARD_CAP_SECONDS = 7200.0

# The TUI sometimes drops the Enter sent right after a paste (seen at TUI startup and
# under multi-second input redraw lag), leaving the prompt unsubmitted in the input
# box. Submission is verified by emptiness transitions of the input box's real
# (non-dim) content — never by matching prompt text, because multi-line pastes render
# only as a "[Pasted text #N +M lines]" placeholder. After each Enter we verify the
# box cleared and re-send only while it is verified non-empty; waits are generous
# because observed cold-start input lag exceeds 60s.
_SUBMIT_VERIFY_WAIT_SECONDS = 1.0
_SUBMIT_RETRY_WAIT_SECONDS = 5.0
_SUBMIT_MAX_ENTER_ATTEMPTS = 4
_PASTE_RENDER_TIMEOUT_SECONDS = 120.0
_CLEAR_INPUT_TIMEOUT_SECONDS = 120.0
_PRE_PROMPT_MENU_MAX_DISMISS = 3
_TUI_MENU_DISMISSED_WARNING = "Warning: dismissed a Claude TUI startup menu with Escape before sending the prompt."


@dataclass(frozen=True)
class ClaudeSubArgs:
  output_format: str
  prompt: str = ""
  model: Optional[str] = None
  effort: Optional[str] = None
  session_id: Optional[str] = None
  resume: Optional[str] = None
  disallowed_tools: list[str] = field(default_factory=list)


@dataclass
class TurnState:
  message_usages: dict[str, dict[str, Any]] = field(default_factory=dict)
  last_usage: dict[str, Any] = field(default_factory=dict)
  last_text: str = ""
  last_stop_reason: Optional[str] = None
  saw_assistant: bool = False

  def observe_assistant(self, message: dict[str, Any]) -> None:
    self.saw_assistant = True
    usage = message.get("usage")
    if isinstance(usage, dict):
      self.last_usage = usage
      message_id = message.get("id")
      if isinstance(message_id, str) and message_id:
        self.message_usages[message_id] = usage
    stop_reason = message.get("stop_reason")
    if isinstance(stop_reason, str):
      self.last_stop_reason = stop_reason
    text = _extract_text(message)
    if text:
      self.last_text = text


class TranscriptTail:

  def __init__(self, path: Path, offset: int) -> None:
    self._file = path.open("rb")
    self._file.seek(offset)
    self._buffer = b""

  def close(self) -> None:
    self._file.close()

  def read_records(self) -> list[dict[str, Any]]:
    data = self._file.read()
    if not data:
      return []
    self._buffer += data
    lines = self._buffer.split(b"\n")
    self._buffer = lines.pop()
    records: list[dict[str, Any]] = []
    for raw in lines:
      if not raw.strip():
        continue
      records.append(json.loads(raw.decode("utf-8")))
    return records


def parse_argv(argv: list[str]) -> ClaudeSubArgs:
  if "--" in argv:
    sep = argv.index("--")
    option_tokens = argv[:sep]
    if argv[sep + 1:]:
      raise ValueError("claude-sub reads the prompt from stdin; positional prompt tokens are not supported")
  else:
    option_tokens = argv

  output_format: Optional[str] = None
  model: Optional[str] = None
  effort: Optional[str] = None
  session_id: Optional[str] = None
  resume: Optional[str] = None
  disallowed_tools: list[str] = []

  i = 0
  while i < len(option_tokens):
    token = option_tokens[i]
    if token in ("-p", "--print", "--verbose", "--dangerously-skip-permissions",
                 "--allow-dangerously-skip-permissions"):
      i += 1
      continue
    if token == "--input-file" or token.startswith("--input-file="):
      raise ValueError("claude-sub does not support --input-file")
    if token in ("--output-format", "--model", "--effort", "--session-id", "--resume", "-r", "--disallowed-tools",
                 "--disallowedTools"):
      if i + 1 >= len(option_tokens):
        raise ValueError(f"{token} requires a value")
      value = option_tokens[i + 1]
      i += 2
    else:
      split = _split_value_flag(token)
      if split is None:
        raise ValueError(f"unsupported claude-sub flag: {token}")
      token, value = split
      i += 1

    if token == "--output-format":
      output_format = value
    elif token == "--model":
      model = value
    elif token == "--effort":
      effort = value
    elif token == "--session-id":
      session_id = value
    elif token in ("--resume", "-r"):
      resume = value
    elif token in ("--disallowed-tools", "--disallowedTools"):
      disallowed_tools.append(value)
    else:
      raise ValueError(f"unsupported claude-sub flag: {token}")

  if output_format != "stream-json":
    raise ValueError("claude-sub only supports --output-format stream-json")
  return ClaudeSubArgs(
      output_format=output_format,
      model=model,
      effort=effort,
      session_id=session_id,
      resume=resume,
      disallowed_tools=disallowed_tools,
  )


def _split_value_flag(token: str) -> Optional[tuple[str, str]]:
  for name in (
      "--output-format",
      "--model",
      "--effort",
      "--session-id",
      "--resume",
      "--disallowed-tools",
      "--disallowedTools",
  ):
    prefix = f"{name}="
    if token.startswith(prefix):
      return name, token[len(prefix):]
  return None


async def _run_tmux_bytes(*args: str, stdin: bytes | None = None, capture: bool = False) -> bytes:
  tmux = _tmux_binary()
  proc = await asyncio.create_subprocess_exec(
      tmux,
      "-L",
      _TMUX_SOCKET,
      *args,
      stdin=asyncio.subprocess.PIPE if stdin is not None else asyncio.subprocess.DEVNULL,
      stdout=asyncio.subprocess.PIPE if capture else asyncio.subprocess.DEVNULL,
      stderr=asyncio.subprocess.PIPE,
      env=_tmux_client_env(),
  )
  stdout, stderr = await proc.communicate(stdin)
  if proc.returncode != 0:
    err = stderr.decode("utf-8", errors="replace").strip()
    raise RuntimeError(f"tmux {' '.join(args)} failed (rc={proc.returncode}): {err}")
  return stdout


async def _capture_pane(session_id: str) -> str:
  output = await _run_tmux_bytes("capture-pane", "-p", "-t", tmux_session_name(session_id), capture=True)
  return output.decode("utf-8", errors="replace")


async def _capture_pane_escapes(session_id: str) -> str:
  """Capture the pane with escape sequences (-e) so dim styling survives for ghost detection."""
  output = await _run_tmux_bytes("capture-pane", "-p", "-e", "-t", tmux_session_name(session_id), capture=True)
  return output.decode("utf-8", errors="replace")


def _emit_tui_menu_dismissed(emit: Any) -> None:
  emit({
      "type": "system",
      "subtype": "tui_menu_dismissed",
      "content": _TUI_MENU_DISMISSED_WARNING,
      "uuid": str(uuid.uuid4()),
  })


async def _dismiss_pre_prompt_menu(session_id: str, emit: Any, dismissed: int) -> int:
  if dismissed >= _PRE_PROMPT_MENU_MAX_DISMISS:
    raise RuntimeError(
        f"Claude TUI startup menu remained before the first prompt after "
        f"{_PRE_PROMPT_MENU_MAX_DISMISS} Escape dismissals for session {session_id}")
  await _run_tmux_bytes("send-keys", "-t", tmux_session_name(session_id), "Escape")
  _emit_tui_menu_dismissed(emit)
  return dismissed + 1


async def _wait_for_prompt_state(session_id: str, emit: Any) -> PaneInputState:
  deadline = time.monotonic() + _PROMPT_READY_TIMEOUT_SECONDS
  dismissed = 0
  while time.monotonic() < deadline:
    state = claude_tui_state.prompt_ready_state(await _capture_pane_escapes(session_id))
    if state.kind == PaneInputKind.PROMPT:
      return state
    if state.kind == PaneInputKind.MENU:
      dismissed = await _dismiss_pre_prompt_menu(session_id, emit, dismissed)
    elif state.kind == PaneInputKind.UNKNOWN:
      pass
    else:
      raise AssertionError(f"unhandled pane input kind: {state.kind!r}")
    await asyncio.sleep(_POLL_SECONDS)
  raise RuntimeError(f"interactive claude prompt did not become ready for session {session_id}")


async def _wait_for_prompt(session_id: str, emit: Any | None = None) -> None:
  if emit is None:
    emit = _emit
  await _wait_for_prompt_state(session_id, emit)


async def _wait_input_box_empty(
    session_id: str,
    timeout: float,
    expected_prompt: str | None = None,
) -> Optional[str]:
  """Poll until the input box's real content reads empty.

  Returns None once empty, or the content still present at timeout — the caller's
  verified-non-empty witness for retrying or failing loud.
  """
  deadline = time.monotonic() + timeout
  while True:
    pane_text = await _capture_pane_escapes(session_id)
    if expected_prompt is None:
      state = claude_tui_state.prompt_ready_state(pane_text)
    else:
      state = claude_tui_state.prompt_send_input_state(pane_text, expected_prompt)
    if state.kind == PaneInputKind.PROMPT:
      if not state.content:
        return None
      if time.monotonic() >= deadline:
        return state.content
    elif state.kind == PaneInputKind.MENU:
      raise RuntimeError(f"Claude TUI startup menu appeared while clearing input for session {session_id}")
    elif state.kind == PaneInputKind.UNKNOWN:
      return None
    else:
      raise AssertionError(f"unhandled pane input kind: {state.kind!r}")
    await asyncio.sleep(_POLL_SECONDS)


async def _send_prompt(session_id: str, prompt: str, emit: Any | None = None) -> None:
  """Paste the prompt and verify submission by emptiness transitions of the input box.

  Prompt text is never matched: a multi-line paste renders only as a
  "[Pasted text #N +M lines]" placeholder, so prefix matching cannot see it. Instead:
  clear any stale real content before pasting (kills the prompt-concatenation hazard),
  wait for the paste to render as non-empty real content, then send Enter and poll for
  the box to empty — re-sending Enter only while the box is verified non-empty so
  stray Enters never reach an empty/ghost box.
  """
  if emit is None:
    emit = _emit
  target = tmux_session_name(session_id)

  state = claude_tui_state.prompt_ready_state(await _capture_pane_escapes(session_id))
  if state.kind == PaneInputKind.MENU:
    state = await _wait_for_prompt_state(session_id, emit)
  elif state.kind == PaneInputKind.UNKNOWN:
    raise RuntimeError(f"Claude TUI prompt was not visible before sending prompt for session {session_id}")
  elif state.kind == PaneInputKind.PROMPT:
    pass
  else:
    raise AssertionError(f"unhandled pane input kind: {state.kind!r}")

  stale = state.content
  if stale:
    await _run_tmux_bytes("send-keys", "-t", target, "C-e")
    await _run_tmux_bytes("send-keys", "-t", target, "C-u")
    leftover = await _wait_input_box_empty(session_id, _CLEAR_INPUT_TIMEOUT_SECONDS)
    if leftover:
      raise RuntimeError(
          f"claude TUI input box held stale content that C-e/C-u did not clear within "
          f"{_CLEAR_INPUT_TIMEOUT_SECONDS:.0f}s for session {session_id}: {leftover!r}")

  buffer_name = f"claude-sub-{session_id[:8]}-{uuid.uuid4().hex[:8]}"
  payload = _BRACKETED_PASTE_START + prompt.encode("utf-8") + _BRACKETED_PASTE_END
  await _run_tmux_bytes("load-buffer", "-b", buffer_name, "-", stdin=payload)
  await _run_tmux_bytes("paste-buffer", "-d", "-b", buffer_name, "-t", target)

  deadline = time.monotonic() + _PASTE_RENDER_TIMEOUT_SECONDS
  while True:
    state = claude_tui_state.prompt_send_input_state(await _capture_pane_escapes(session_id), prompt)
    if state.kind == PaneInputKind.PROMPT:
      if state.content:
        break
    elif state.kind == PaneInputKind.MENU:
      raise RuntimeError(f"Claude TUI startup menu appeared while waiting for pasted prompt for session {session_id}")
    elif state.kind == PaneInputKind.UNKNOWN:
      pass
    else:
      raise AssertionError(f"unhandled pane input kind: {state.kind!r}")
    if time.monotonic() >= deadline:
      raise RuntimeError(
          f"pasted prompt did not render in the claude TUI input box within "
          f"{_PASTE_RENDER_TIMEOUT_SECONDS:.0f}s for session {session_id}")
    await asyncio.sleep(_POLL_SECONDS)

  content: Optional[str] = None
  for attempt in range(_SUBMIT_MAX_ENTER_ATTEMPTS):
    await _run_tmux_bytes("send-keys", "-t", target, "Enter")
    content = await _wait_input_box_empty(
        session_id,
        _SUBMIT_VERIFY_WAIT_SECONDS if attempt == 0 else _SUBMIT_RETRY_WAIT_SECONDS,
        prompt,
    )
    if content is None:
      return
  raise RuntimeError(
      f"claude TUI input box still holds unsubmitted content after {_SUBMIT_MAX_ENTER_ATTEMPTS} Enter attempts "
      f"for session {session_id}: {content!r}")


async def _interrupt_turn(session_id: str) -> None:
  await _run_tmux_bytes("send-keys", "-t", tmux_session_name(session_id), "C-c")


async def _wait_for_transcript_path(session_id: str) -> Path:
  deadline = time.monotonic() + _TRANSCRIPT_PATH_TIMEOUT_SECONDS
  while time.monotonic() < deadline:
    path = _find_existing_claude_jsonl(session_id)
    if path is not None:
      return path
    await asyncio.sleep(_POLL_SECONDS)
  raise RuntimeError(f"interactive claude transcript not found for session {session_id}")


async def _validate_resume_target(session_id: str) -> None:
  if _find_existing_claude_jsonl(session_id) is not None:
    return
  if await tmux_session_exists(session_id):
    return
  raise RuntimeError(f"resume session not found: {session_id}")


def _emit(event: dict[str, Any]) -> None:
  sys.stdout.write(json.dumps(event, ensure_ascii=False, separators=(",", ":")) + "\n")
  sys.stdout.flush()


def _init_event(args: ClaudeSubArgs, session_id: str) -> dict[str, Any]:
  return {
      "type": "system",
      "subtype": "init",
      "cwd": str(Path.cwd()),
      "session_id": session_id,
      "tools": [],
      "mcp_servers": [],
      "model": args.model or "",
      "permissionMode": "bypassPermissions",
      "output_style": "default",
      "agents": [],
      "skills": [],
      "plugins": [],
      "uuid": str(uuid.uuid4()),
  }


def _convert_record(record: dict[str, Any], session_id: str) -> Optional[dict[str, Any]]:
  record_type = record.get("type")
  if record_type == "assistant":
    message = record.get("message")
    if not isinstance(message, dict):
      return None
    event = {
        "type": "assistant",
        "message": message,
        "parent_tool_use_id": record.get("parent_tool_use_id"),
        "session_id": record.get("sessionId") or session_id,
        "uuid": record.get("uuid") or str(uuid.uuid4()),
    }
    request_id = record.get("requestId") or record.get("request_id")
    if request_id:
      event["request_id"] = request_id
    return event

  if record_type == "user":
    message = record.get("message")
    if not isinstance(message, dict):
      return None
    if isinstance(message.get("content"), str):
      return None
    event = {
        "type": "user",
        "message": message,
        "parent_tool_use_id": record.get("parent_tool_use_id"),
        "session_id": record.get("sessionId") or session_id,
        "uuid": record.get("uuid") or str(uuid.uuid4()),
    }
    tool_result = record.get("toolUseResult") or record.get("tool_use_result")
    if tool_result is not None:
      event["tool_use_result"] = tool_result
    return event

  return None


def _extract_text(message: dict[str, Any]) -> str:
  content = message.get("content")
  if not isinstance(content, list):
    return ""
  parts: list[str] = []
  for item in content:
    if isinstance(item, dict) and item.get("type") == "text" and isinstance(item.get("text"), str):
      parts.append(item["text"])
  return "".join(parts)


def _aggregate_usage(state: TurnState) -> dict[str, Any]:
  if not state.message_usages:
    return dict(state.last_usage)
  if len(state.message_usages) == 1:
    return dict(next(iter(state.message_usages.values())))

  usage = dict(state.last_usage)
  for key in ("input_tokens", "output_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"):
    usage[key] = sum(int(u.get(key) or 0) for u in state.message_usages.values())
  iterations: list[dict[str, Any]] = []
  for item in state.message_usages.values():
    item_iterations = item.get("iterations")
    if isinstance(item_iterations, list):
      iterations.extend(dict(iteration) for iteration in item_iterations if isinstance(iteration, dict))
      continue
    iteration = {
        "input_tokens": item.get("input_tokens", 0),
        "output_tokens": item.get("output_tokens", 0),
        "cache_read_input_tokens": item.get("cache_read_input_tokens", 0),
        "cache_creation_input_tokens": item.get("cache_creation_input_tokens", 0),
        "type": "message",
    }
    if "cache_creation" in item:
      iteration["cache_creation"] = item["cache_creation"]
    iterations.append(iteration)
  usage["iterations"] = iterations
  return usage


def _result_event(session_id: str, state: TurnState, turn_duration: dict[str, Any]) -> dict[str, Any]:
  duration_ms = int(turn_duration.get("durationMs") or 0)
  return {
      "type": "result",
      "subtype": "success",
      "is_error": False,
      "api_error_status": None,
      "duration_ms": duration_ms,
      "duration_api_ms": duration_ms,
      "num_turns": max(1, len(state.message_usages)),
      "result": state.last_text,
      "stop_reason": state.last_stop_reason or "end_turn",
      "session_id": session_id,
      "total_cost_usd": 0,
      "usage": _aggregate_usage(state),
      "permission_denials": [],
      "terminal_reason": "completed",
      "uuid": str(uuid.uuid4()),
  }


async def _stream_turn(args: ClaudeSubArgs, stop_event: asyncio.Event) -> None:
  session_id = args.resume or args.session_id or str(uuid.uuid4())
  if args.resume:
    await _validate_resume_target(session_id)
  with contextlib.redirect_stdout(io.StringIO()):
    await ensure_tmux_session(
        session_id,
        Path.cwd(),
        model=args.model,
        effort=args.effort,
        disallowed_tools=args.disallowed_tools,
    )

  _emit(_init_event(args, session_id))
  await _wait_for_prompt(session_id, _emit)
  existing_jsonl = _find_existing_claude_jsonl(session_id)
  offset = existing_jsonl.stat().st_size if existing_jsonl is not None else 0

  await _send_prompt(session_id, args.prompt, _emit)

  jsonl_path = existing_jsonl or await _wait_for_transcript_path(session_id)
  tail = TranscriptTail(jsonl_path, offset)
  state = TurnState()
  turn_start = time.monotonic()
  last_activity = turn_start
  last_probe = turn_start
  menu_probes = 0
  try:
    while True:
      if stop_event.is_set():
        await _interrupt_turn(session_id)
        raise RuntimeError("terminated")

      records = tail.read_records()
      if records:
        last_activity = time.monotonic()
        menu_probes = 0
      for record in records:
        if record.get("type") == "assistant" and isinstance(record.get("message"), dict):
          state.observe_assistant(record["message"])

        event = _convert_record(record, session_id)
        if event is not None:
          _emit(event)

        if record.get("type") == "system" and record.get("subtype") == "turn_duration" and state.saw_assistant:
          _emit(_result_event(session_id, state, record))
          return

      now = time.monotonic()
      if now - last_activity >= _STALL_PROBE_SECONDS and now - last_probe >= _PANE_PROBE_INTERVAL_SECONDS:
        last_probe = now
        if claude_tui_state.quiet_turn_has_blocking_menu(await _capture_pane_escapes(session_id), args.prompt):
          menu_probes += 1
          if menu_probes >= _MENU_CONFIRM_PROBES:
            await _interrupt_turn(session_id)
            raise RuntimeError(
                "interactive TUI menu detected (AskUserQuestion / plan-approval); claude-sub cannot answer it")
        else:
          menu_probes = 0

      if now - turn_start >= _TURN_HARD_CAP_SECONDS:
        await _interrupt_turn(session_id)
        raise RuntimeError(f"turn exceeded {_TURN_HARD_CAP_SECONDS:.0f}s without completing")

      await asyncio.sleep(_POLL_SECONDS)
  finally:
    tail.close()


async def _run(args: ClaudeSubArgs) -> None:
  stop_event = asyncio.Event()
  loop = asyncio.get_running_loop()
  for sig in (signal.SIGINT, signal.SIGTERM):
    loop.add_signal_handler(sig, stop_event.set)
  await _stream_turn(args, stop_event)


def main(argv: Optional[list[str]] = None) -> int:
  try:
    args = parse_argv(sys.argv[1:] if argv is None else argv)
    args = replace(args, prompt=sys.stdin.read())
    asyncio.run(_run(args))
    return 0
  except Exception as e:
    print(f"claude-sub: {e}", file=sys.stderr)
    return 1


if __name__ == "__main__":
  raise SystemExit(main())
