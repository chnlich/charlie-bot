"""Subscription-backed drop-in for the interactive Claude Code CLI.

``claude-sub`` keeps Claude's interactive display in a dedicated tmux pane, but all
prompt delivery and semantic events travel through official command hooks.  The tmux
pane is a display/lifecycle host only: this module never reads its screen or transcript
and never writes terminal input.
"""

from __future__ import annotations

import asyncio
import json
import os
import re
import shutil
import signal
import sys
import tempfile
import time
import uuid
from dataclasses import dataclass, field
from enum import StrEnum
from pathlib import Path
from typing import Any

from src.agents.backends.base import SKIP_PERMISSIONS_FLAG, SKIP_PERMISSIONS_SETTINGS
from src.agents.backends.claude_code import headless_claude_env
from src.agents.backends.pty_common import (
  _TMUX_SOCKET,
  _tmux_binary,
  _tmux_client_env,
  tmux_session_exists,
  tmux_session_name,
)
from src.agents.backends.tui import mark_project_trusted
from src.cli.claude_sub_bridge import (
  HookBridge,
  HookTurnState,
  PromptDelivery,
)
from src.core.config import charliebot_home_dir
from src.core.json_utils import write_json_atomically
from src.core.process import kill_process_group

_MIN_CLAUDE_VERSION = (2, 1, 210)
_TARGET_CLAUDE_VERSION = (2, 1, 211)
_MAX_ARG_STRLEN = 128 * 1024
_ARGV_SAFETY_MARGIN = 4096
MAX_PROMPT_BYTES = _MAX_ARG_STRLEN - _ARGV_SAFETY_MARGIN
_TMUX_COLS = 120
_TMUX_ROWS = 40
_POLL_SECONDS = 0.1
_PROCESS_PROBE_INTERVAL_SECONDS = 0.5
_SUBMISSION_CONFIRMATION_TIMEOUT_SECONDS = 30.0
_TURN_TIMEOUT_SECONDS = 7200.0
_TERMINATE_TIMEOUT_SECONDS = 5.0
_IDLE_NOTIFICATION_THRESHOLD_MS = 1000
_MANAGED_NOTIFICATION_SETTINGS: dict[str, Any] = {
    "messageIdleNotifThresholdMs": _IDLE_NOTIFICATION_THRESHOLD_MS,
    "inputNeededNotifEnabled": True,
    "preferredNotifChannel": "terminal_bell",
}
_CAPABILITY_MARKERS = ("--plugin-dir", "--settings", "--session-id", "--resume")
_VERSION_RE = re.compile(r"(?P<major>\d+)\.(?P<minor>\d+)\.(?P<patch>\d+)")
_SESSION_CONFIG_DIR_NAME = "configs"
_HOOK_EVENTS = (
    "SessionStart",
    "UserPromptSubmit",
    "MessageDisplay",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "PostCompact",
    "Stop",
    "StopFailure",
    "Notification",
    "PermissionRequest",
    "SessionEnd",
)


class ClaudeSubError(RuntimeError):
  """A visible claude-sub protocol or lifecycle failure."""


class UnknownPromptDeliveryError(ClaudeSubError):
  """The process may have received a prompt without confirming it."""


class SessionMarkerState(StrEnum):
  NEW = "new"
  STARTED_BY_NEW_ADAPTER = "started-by-new-adapter"
  MIGRATION_BLOCKED = "migration-blocked"


@dataclass(frozen=True)
class ClaudeSubArgs:
  output_format: str
  prompt: str = ""
  model: str | None = None
  effort: str | None = None
  session_id: str | None = None
  resume: str | None = None
  disallowed_tools: list[str] = field(default_factory=list)
  settings: list[str] = field(default_factory=list)


@dataclass(frozen=True)
class PaneInfo:
  pid: int
  cwd: str
  command: str
  dead: bool

  @property
  def is_claude(self) -> bool:
    command = Path(self.command).name.lower()
    return command in {"claude", "claude-code"} or "claude" in command


def parse_argv(argv: list[str]) -> ClaudeSubArgs:
  if "--" in argv:
    sep = argv.index("--")
    option_tokens = argv[:sep]
    if argv[sep + 1:]:
      raise ValueError("claude-sub reads the prompt from stdin; positional prompt tokens are not supported")
  else:
    option_tokens = argv

  output_format: str | None = None
  model: str | None = None
  effort: str | None = None
  session_id: str | None = None
  resume: str | None = None
  disallowed_tools: list[str] = []
  settings: list[str] = []

  i = 0
  while i < len(option_tokens):
    token = option_tokens[i]
    if token in (
        "-p",
        "--print",
        "--verbose",
        SKIP_PERMISSIONS_FLAG,
        "--allow-dangerously-skip-permissions",
    ):
      i += 1
      continue
    if token == "--input-file" or token.startswith("--input-file="):
      raise ValueError("claude-sub does not support --input-file")
    if token in (
        "--output-format",
        "--model",
        "--effort",
        "--session-id",
        "--resume",
        "-r",
        "--disallowed-tools",
        "--disallowedTools",
        "--settings",
    ):
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
    elif token == "--settings":
      settings.append(value)
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
      settings=settings,
  )


def _split_value_flag(token: str) -> tuple[str, str] | None:
  for name in (
      "--output-format",
      "--model",
      "--effort",
      "--session-id",
      "--resume",
      "--disallowed-tools",
      "--disallowedTools",
      "--settings",
  ):
    prefix = f"{name}="
    if token.startswith(prefix):
      return name, token[len(prefix):]
  return None


def validate_prompt(prompt: str) -> None:
  """Validate the host-level constraints for one direct argv prompt."""
  if "\x00" in prompt:
    raise ValueError("prompt contains an embedded NUL byte and cannot be passed as one argv element")
  prompt_bytes = prompt.encode("utf-8")
  if len(prompt_bytes) > MAX_PROMPT_BYTES:
    raise ValueError(
        f"prompt is {len(prompt_bytes)} UTF-8 bytes, exceeding the {MAX_PROMPT_BYTES}-byte one-argv-element "
        f"limit (Linux MAX_ARG_STRLEN is {_MAX_ARG_STRLEN} bytes; {_ARGV_SAFETY_MARGIN} bytes are reserved "
        "for command-line safety)")
  # Claude Code 2.1.212 accepts `--` as the documented command-line separator.  The
  # launch builder always inserts it before this value, including for a leading '-'.


async def _run_tmux(*args: str, capture: bool = False) -> tuple[int, str]:
  tmux = _tmux_binary()
  env = _tmux_client_env()
  env.pop("TMUX", None)
  with tempfile.TemporaryFile() as stderr_file:
    proc = await asyncio.create_subprocess_exec(
        tmux,
        "-L",
        _TMUX_SOCKET,
        *args,
        stdin=asyncio.subprocess.DEVNULL,
        stdout=asyncio.subprocess.PIPE if capture else asyncio.subprocess.DEVNULL,
        stderr=stderr_file,
        env=env,
    )
    stdout, _ = await proc.communicate()
    stderr_file.seek(0)
    error_text = stderr_file.read().decode("utf-8", errors="replace").strip()
  output = stdout.decode("utf-8", errors="replace") if stdout else ""
  return proc.returncode or 0, output if capture and proc.returncode == 0 else error_text or output


async def _tmux_checked(*args: str, capture: bool = False) -> str:
  rc, output = await _run_tmux(*args, capture=capture)
  if rc != 0:
    raise ClaudeSubError(f"tmux {' '.join(args)} failed (rc={rc}): {output.strip()}")
  return output


async def _pane_info(session_id: str) -> PaneInfo:
  target = tmux_session_name(session_id)
  output = await _tmux_checked(
      "display-message",
      "-p",
      "-t",
      target,
      "#{pane_pid}\t#{pane_current_path}\t#{pane_current_command}\t#{pane_dead}",
      capture=True,
  )
  fields = output.rstrip("\n").split("\t")
  if len(fields) != 4:
    raise ClaudeSubError(f"tmux pane metadata was malformed for {target}: {output!r}")
  try:
    pid = int(fields[0])
  except ValueError as error:
    raise ClaudeSubError(f"tmux pane metadata had a non-numeric pid for {target}: {fields[0]!r}") from error
  if fields[3] not in {"0", "1"}:
    raise ClaudeSubError(f"tmux pane metadata had an unknown pane_dead value: {fields[3]!r}")
  return PaneInfo(pid=pid, cwd=fields[1], command=fields[2], dead=fields[3] == "1")


def _session_marker_dir() -> Path:
  """This profile's claude-sub marker directory. Resolved per call, never at import."""
  return charliebot_home_dir() / "claude-sub-sessions"


def _marker_path(session_id: str) -> Path:
  return _session_marker_dir() / f"{session_id}.json"


def _read_marker(session_id: str) -> SessionMarkerState | None:
  path = _marker_path(session_id)
  if not path.exists():
    return None
  try:
    data = json.loads(path.read_text(encoding="utf-8"))
  except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
    raise ClaudeSubError(f"claude-sub session marker is unreadable: {path}: {error}") from error
  if not isinstance(data, dict) or set(data) != {"state"} or not isinstance(data.get("state"), str):
    raise ClaudeSubError(f"claude-sub session marker has an invalid shape: {path}")
  try:
    return SessionMarkerState(data["state"])
  except ValueError as error:
    raise ClaudeSubError(f"claude-sub session marker has an unknown state: {data['state']!r}") from error


def _write_marker(session_id: str, state: SessionMarkerState) -> None:
  path = _marker_path(session_id)
  path.parent.mkdir(parents=True, exist_ok=True)
  write_json_atomically(path, {"state": state.value}, newline=True)


def _claude_user_config_paths() -> tuple[Path, Path, Path, Path]:
  """Return the active Claude global, user-settings, credentials, and remote paths."""
  configured_root = os.environ.get("CLAUDE_CONFIG_DIR")
  if configured_root:
    root = Path(configured_root).expanduser()
    settings_root = root
  else:
    root = Path.home()
    settings_root = root / ".claude"
  return (
      root / ".claude.json",
      settings_root / "settings.json",
      settings_root / ".credentials.json",
      settings_root / "remote-settings.json",
  )


def _session_config_dir(session_id: str) -> Path:
  return _session_marker_dir() / _SESSION_CONFIG_DIR_NAME / session_id


def _write_json_atomically(path: Path, value: dict[str, Any]) -> None:
  write_json_atomically(path, value, newline=True, private=True)


def _read_json_object(path: Path, description: str) -> dict[str, Any]:
  try:
    value = json.loads(path.read_text(encoding="utf-8"))
  except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
    raise ClaudeSubError(f"{description} is unreadable: {path}: {error}") from error
  if not isinstance(value, dict):
    raise ClaudeSubError(f"{description} must contain a JSON object: {path}")
  return value


def _copy_session_file(source: Path, target: Path, description: str) -> None:
  try:
    shutil.copyfile(source, target)
  except OSError as error:
    raise ClaudeSubError(f"could not copy {description} into the session-only Claude config: {error}") from error
  os.chmod(target, 0o600)


def _prepare_session_config(session_id: str, cwd: Path) -> Path:
  """Create the persistent per-session Claude config overlay without touching user files."""
  config_dir = _session_config_dir(session_id)
  try:
    config_dir.mkdir(parents=True, exist_ok=True)
  except OSError as error:
    raise ClaudeSubError(f"could not create the session-only Claude config directory {config_dir}: {error}") from error

  global_source, settings_source, credentials_source, remote_source = _claude_user_config_paths()
  global_target = config_dir / ".claude.json"
  if global_target.exists():
    global_settings = _read_json_object(global_target, "session-only Claude global state")
  else:
    if not global_source.is_file():
      raise ClaudeSubError(f"Claude global state is missing; cannot create session config overlay: {global_source}")
    _copy_session_file(global_source, global_target, "Claude global state")
    global_settings = _read_json_object(global_target, "session-only Claude global state")
  # Seed folder trust for this turn's cwd so the interactive TUI does not park on
  # the trust dialog (which would stall hooks until the submission-confirmation
  # timeout).  Only this session overlay changes; the user's ~/.claude.json stays
  # untouched.
  trust_changed = mark_project_trusted(global_settings, str(cwd))
  # Claude 2.1.211/2.1.212 reads this notification setting from global state rather
  # than the --settings flag.  Keep the override in this session-only copy so the
  # idle hook remains prompt without modifying ~/.claude.json.  Combine the trust
  # and idle changes so one prepare writes .claude.json at most once, only when
  # something actually changed.
  idle_changed = global_settings.get("messageIdleNotifThresholdMs") != _IDLE_NOTIFICATION_THRESHOLD_MS
  global_settings["messageIdleNotifThresholdMs"] = _IDLE_NOTIFICATION_THRESHOLD_MS
  if trust_changed or idle_changed:
    _write_json_atomically(global_target, global_settings)

  if settings_source.is_file() and not (config_dir / "settings.json").exists():
    _copy_session_file(settings_source, config_dir / "settings.json", "user Claude settings")
  if remote_source.is_file() and not (config_dir / "remote-settings.json").exists():
    _copy_session_file(remote_source, config_dir / "remote-settings.json", "Claude remote settings")
  credentials_target = config_dir / ".credentials.json"
  if not credentials_target.exists():
    if not credentials_source.is_file():
      raise ClaudeSubError(f"Claude subscription credentials are missing: {credentials_source}")
    _copy_session_file(credentials_source, credentials_target, "Claude credentials")
  elif credentials_source.is_file() and credentials_source.stat().st_mtime > credentials_target.stat().st_mtime:
    # Claude rotates OAuth tokens and rewrites ~/.claude/.credentials.json; a
    # long-lived overlay must not start a turn with a stale snapshot.  Refresh
    # only when the source is strictly newer so an unchanged overlay is untouched.
    _copy_session_file(credentials_source, credentials_target, "Claude credentials")
  return config_dir


async def _create_tmux_host(session_id: str, cwd: Path) -> None:
  name = tmux_session_name(session_id)
  await _tmux_checked(
      "new-session",
      "-d",
      "-s",
      name,
      "-x",
      str(_TMUX_COLS),
      "-y",
      str(_TMUX_ROWS),
      "-c",
      str(cwd),
      "sleep",
      "infinity",
  )
  await _tmux_checked("set-option", "-t", name, "history-limit", "50000")
  await _tmux_checked("set-window-option", "-t", name, "remain-on-exit", "on")


async def _prepare_tmux_session(session_id: str, cwd: Path, requested_resume: bool) -> bool:
  """Validate the marker/pane binding and return whether Claude must be resumed."""
  marker_state = _read_marker(session_id)
  exists = await tmux_session_exists(session_id)
  if not exists:
    if marker_state is not None and marker_state not in (
        SessionMarkerState.NEW,
        SessionMarkerState.MIGRATION_BLOCKED,
    ):
      raise ClaudeSubError(
          f"claude-sub session {session_id} has a {marker_state.value} marker but no live tmux pane; "
          "refusing to resume without verifying its bound working directory")
    await _create_tmux_host(session_id, cwd)
    if marker_state == SessionMarkerState.MIGRATION_BLOCKED:
      # The old-style TUI was exited (old sessions lacked remain-on-exit and
      # closed); the cwd was already verified when the marker was written, so
      # resume the same Claude session id.  Keep the marker until the turn
      # launches, then _stream_turn flips it to STARTED_BY_NEW_ADAPTER.
      return True
    _write_marker(session_id, SessionMarkerState.NEW)
    return requested_resume

  info = await _pane_info(session_id)
  if info.cwd and os.path.realpath(info.cwd) != os.path.realpath(str(cwd)):
    raise ClaudeSubError(
        f"claude-sub session {session_id} is bound to cwd {info.cwd}, not {cwd}; refusing cross-cwd resume")

  if marker_state is None:
    if not info.dead and info.is_claude:
      _write_marker(session_id, SessionMarkerState.MIGRATION_BLOCKED)
      raise ClaudeSubError(
          f"claude-sub found an old-style live Claude TUI in tmux session {session_id}; "
          "exit the old TUI before submitting through the new hook bridge")
    return True

  if marker_state == SessionMarkerState.MIGRATION_BLOCKED and not info.dead and info.is_claude:
    raise ClaudeSubError(
        f"claude-sub migration is blocked by the old live Claude TUI in session {session_id}; "
        "exit the old TUI before submitting")
  if marker_state == SessionMarkerState.NEW:
    return requested_resume
  return True


def _session_settings(args: ClaudeSubArgs) -> str:
  merged: dict[str, Any] = {
      **SKIP_PERMISSIONS_SETTINGS,
      **_MANAGED_NOTIFICATION_SETTINGS,
  }
  for raw in args.settings:
    try:
      value = json.loads(raw)
    except json.JSONDecodeError as error:
      raise ClaudeSubError(f"claude-sub --settings must be inline JSON, got {raw!r}") from error
    if not isinstance(value, dict):
      raise ClaudeSubError("claude-sub --settings must contain a JSON object")
    merged.update(value)
  # Inline managed settings must be last so the completion threshold cannot be
  # overridden by the fast-mode setting passed by ClaudeCodeBackend.
  merged.update(_MANAGED_NOTIFICATION_SETTINGS)
  return json.dumps(merged, ensure_ascii=False, separators=(",", ":"))


def _disallowed_tool_value(values: list[str]) -> str:
  return ",".join(values)


def _build_claude_argv(
    args: ClaudeSubArgs,
    session_id: str,
    resume: bool,
    plugin_dir: Path,
) -> list[str]:
  argv = [
      "claude",
      "--settings",
      _session_settings(args),
      SKIP_PERMISSIONS_FLAG,
      "--plugin-dir",
      str(plugin_dir),
      "--resume" if resume else "--session-id",
      session_id,
  ]
  if args.model:
    argv.extend(["--model", args.model])
  if args.effort:
    argv.extend(["--effort", args.effort])
  if args.disallowed_tools:
    argv.extend(["--disallowed-tools", _disallowed_tool_value(args.disallowed_tools)])
  # Claude Code 2.1.212 accepts `--` and treats the following value as the prompt,
  # even when it starts with '-'.  tmux respawn-pane passes these argv entries
  # directly to Claude; it does not invoke a shell for the command after the target.
  argv.extend(["--", args.prompt])
  return argv


async def _run_cli_capture(*args: str) -> tuple[int, str, str]:
  binary = shutil.which(args[0]) if args else None
  if binary is None:
    raise ClaudeSubError(f"required CLI binary not found: {args[0] if args else '<empty command>'}")
  proc = await asyncio.create_subprocess_exec(
      binary,
      *args[1:],
      stdin=asyncio.subprocess.DEVNULL,
      stdout=asyncio.subprocess.PIPE,
      stderr=asyncio.subprocess.PIPE,
      env=_tmux_client_env(),
  )
  stdout, stderr = await proc.communicate()
  return (
      proc.returncode or 0,
      stdout.decode("utf-8", errors="replace"),
      stderr.decode("utf-8", errors="replace"),
  )


async def _check_cli_capabilities() -> None:
  rc, stdout, stderr = await _run_cli_capture("claude", "--version")
  if rc != 0:
    raise ClaudeSubError(f"Claude Code version check failed (rc={rc}): {stderr.strip()}")
  match = _VERSION_RE.search(stdout)
  if match is None:
    raise ClaudeSubError(f"Claude Code version output did not contain a semantic version: {stdout.strip()!r}")
  version = tuple(int(match.group(name)) for name in ("major", "minor", "patch"))
  if version < _MIN_CLAUDE_VERSION:
    raise ClaudeSubError(
        f"Claude Code {'.'.join(str(part) for part in version)} is below the minimum compatible version "
        f"{'.'.join(str(part) for part in _MIN_CLAUDE_VERSION)} (target {'.'.join(str(part) for part in _TARGET_CLAUDE_VERSION)})")

  rc, stdout, stderr = await _run_cli_capture("claude", "--help", "--", "claude-sub-leading-dash-probe")
  if rc != 0:
    raise ClaudeSubError(f"Claude Code capability check failed (rc={rc}): {stderr.strip()}")
  missing = [marker for marker in _CAPABILITY_MARKERS if marker not in stdout]
  if missing or "Arguments:" not in stdout or "prompt" not in stdout:
    missing_text = ", ".join(missing) if missing else "prompt argument/-- separator"
    raise ClaudeSubError(f"Claude Code lacks required claude-sub capabilities: {missing_text}")


def _write_hook_plugin(root: Path, bridge: HookBridge) -> Path:
  plugin_dir = root / "plugin"
  hooks_dir = plugin_dir / "hooks"
  hooks_dir.mkdir(parents=True, exist_ok=True)
  (plugin_dir / ".claude-plugin").mkdir(parents=True, exist_ok=True)
  (plugin_dir / ".claude-plugin" / "plugin.json").write_text(
      json.dumps(
          {
              "name": "charliebot-hook-bridge",
              "version": "0.1.0",
              "description": "Session-only CharlieBot Claude Code hook bridge",
              "author": {
                  "name": "CharlieBot",
              },
          },
          separators=(",", ":"),
      ) + "\n",
      encoding="utf-8",
  )
  helper = Path(__file__).with_name("claude_sub_hook.py").resolve()
  hooks: dict[str, list[dict[str, Any]]] = {}
  for event_name in _HOOK_EVENTS:
    gate = event_name in {"UserPromptSubmit", "PreToolUse", "PermissionRequest"}
    hook_args = [
        str(helper),
        "--socket",
        str(bridge.socket_path),
        "--token",
        bridge.token,
    ]
    if gate:
      hook_args.append("--gate")
    command: dict[str, Any] = {
        "type": "command",
        "command": sys.executable,
        "args": hook_args,
    }
    if event_name == "UserPromptSubmit":
      command["timeout"] = int(_SUBMISSION_CONFIRMATION_TIMEOUT_SECONDS)
    group: dict[str, Any] = {"hooks": [command]}
    hooks[event_name] = [group]
  (hooks_dir / "hooks.json").write_text(
      json.dumps({"description": "CharlieBot session-only hook bridge", "hooks": hooks}, separators=(",", ":")) +
      "\n",
      encoding="utf-8",
  )
  return plugin_dir


async def _validate_hook_plugin(plugin_dir: Path) -> None:
  rc, stdout, stderr = await _run_cli_capture("claude", "plugin", "validate", "--strict", str(plugin_dir))
  if rc != 0:
    detail = stderr.strip() or stdout.strip()
    raise ClaudeSubError(f"Claude Code hook capability/plugin validation failed (rc={rc}): {detail}")


async def _respawn_claude(
    args: ClaudeSubArgs,
    session_id: str,
    resume: bool,
    plugin_dir: Path,
    cwd: Path,
    config_dir: Path | None = None,
) -> None:
  tmux_args: list[str] = [
      "respawn-pane",
      "-k",
      "-t",
      tmux_session_name(session_id),
      "-c",
      str(cwd),
  ]
  for key, value in headless_claude_env().items():
    tmux_args.extend(["-e", f"{key}={value}"])
  if config_dir is not None:
    tmux_args.extend(["-e", f"CLAUDE_CONFIG_DIR={config_dir}"])
  tmux_args.extend(_build_claude_argv(args, session_id, resume, plugin_dir))
  await _tmux_checked(*tmux_args)


async def _terminate_foreground(session_id: str) -> None:
  info = await _pane_info(session_id)
  if info.dead or not info.is_claude:
    return
  kill_process_group(info.pid, signal.SIGTERM)
  deadline = time.monotonic() + _TERMINATE_TIMEOUT_SECONDS
  while time.monotonic() < deadline:
    current = await _pane_info(session_id)
    if current.dead or not current.is_claude:
      return
    await asyncio.sleep(_POLL_SECONDS)
  current = await _pane_info(session_id)
  if not current.dead and current.is_claude:
    kill_process_group(current.pid, signal.SIGKILL)
    kill_deadline = time.monotonic() + _TERMINATE_TIMEOUT_SECONDS
    while time.monotonic() < kill_deadline:
      current = await _pane_info(session_id)
      if current.dead or not current.is_claude:
        return
      await asyncio.sleep(_POLL_SECONDS)
    raise ClaudeSubError(f"Claude foreground process did not exit after SIGKILL in session {session_id}")


def _emit(event: dict[str, Any]) -> None:
  sys.stdout.write(json.dumps(event, ensure_ascii=False, separators=(",", ":")) + "\n")
  sys.stdout.flush()


def _error_event(message: str) -> dict[str, Any]:
  return {
      "type": "error",
      "message": message,
      "content": message,
      "is_error": True,
      "uuid": str(uuid.uuid4()),
  }


def _result_event(session_id: str, candidate: str, duration_ms: int) -> dict[str, Any]:
  return {
      "type": "result",
      "subtype": "success",
      "is_error": False,
      "api_error_status": None,
      "duration_ms": duration_ms,
      "duration_api_ms": duration_ms,
      "num_turns": 1,
      "result": candidate,
      "stop_reason": "end_turn",
      "session_id": session_id,
      "total_cost_usd": None,
      "usage": {},
      "permission_denials": [],
      "terminal_reason": "completed",
      "uuid": str(uuid.uuid4()),
  }


def _unknown_delivery_message(reason: str) -> str:
  return (
      f"{reason}; prompt delivery is UNKNOWN: the prompt MAY already have been received by Claude; "
      "claude-sub terminated the foreground process and will never retry or replay this prompt automatically"
  )


async def _stream_turn(args: ClaudeSubArgs, stop_event: asyncio.Event) -> None:
  validate_prompt(args.prompt)
  session_id = args.resume or args.session_id or str(uuid.uuid4())
  cwd = Path.cwd().resolve()
  requested_resume = args.resume is not None
  resume = await _prepare_tmux_session(session_id, cwd, requested_resume)
  config_dir = _prepare_session_config(session_id, cwd)

  with tempfile.TemporaryDirectory(prefix=f"claude-sub-{session_id[:8]}-") as temporary_dir:
    temporary_root = Path(temporary_dir)
    socket_path = temporary_root / "bridge.sock"
    state = HookTurnState(
        expected_session_id=session_id,
        expected_cwd=str(cwd),
        expected_prompt=args.prompt,
        model=args.model or "",
        expected_source="resume" if resume else "startup",
    )
    bridge = HookBridge(socket_path, uuid.uuid4().hex, state)
    await bridge.start()
    launch_attempted = False
    terminated = False
    completed = False
    try:
      plugin_dir = _write_hook_plugin(temporary_root, bridge)
      await _validate_hook_plugin(plugin_dir)
      # The bridge and its session-only plugin are both ready before this single
      # process-level respawn.  No empty Claude launch is used as a readiness probe.
      launch_attempted = True
      turn_start = time.monotonic()
      await _respawn_claude(args, session_id, resume, plugin_dir, cwd, config_dir)
      _write_marker(session_id, SessionMarkerState.STARTED_BY_NEW_ADAPTER)
      confirmation_deadline = turn_start + _SUBMISSION_CONFIRMATION_TIMEOUT_SECONDS
      turn_deadline = turn_start + _TURN_TIMEOUT_SECONDS
      last_process_probe = turn_start
      while True:
        if stop_event.is_set():
          await _terminate_foreground(session_id)
          terminated = True
          raise ClaudeSubError("claude-sub cancelled; the current Claude foreground process was terminated")

        while not bridge.events.empty():
          _emit(bridge.events.get_nowait())

        if bridge.failure is not None:
          if state.delivery != PromptDelivery.ACKNOWLEDGED:
            await _terminate_foreground(session_id)
            terminated = True
            raise UnknownPromptDeliveryError(_unknown_delivery_message(str(bridge.failure)))
          await _terminate_foreground(session_id)
          terminated = True
          raise ClaudeSubError(str(bridge.failure))

        if state.delivery == PromptDelivery.ACKNOWLEDGED and state.stop_seen and state.idle_seen:
          _emit(_result_event(session_id, state.stop_candidate or "", int((time.monotonic() - turn_start) * 1000)))
          completed = True
          return

        now = time.monotonic()
        if state.delivery != PromptDelivery.ACKNOWLEDGED and now >= confirmation_deadline:
          await _terminate_foreground(session_id)
          terminated = True
          raise UnknownPromptDeliveryError(_unknown_delivery_message("submission confirmation timeout"))
        if now >= turn_deadline:
          await _terminate_foreground(session_id)
          terminated = True
          raise ClaudeSubError(f"overall turn timeout after {_TURN_TIMEOUT_SECONDS:.0f}s")
        if now - last_process_probe >= _PROCESS_PROBE_INTERVAL_SECONDS:
          last_process_probe = now
          info = await _pane_info(session_id)
          if info.dead:
            if state.delivery != PromptDelivery.ACKNOWLEDGED:
              raise UnknownPromptDeliveryError(
                  _unknown_delivery_message("Claude foreground process exited before submission confirmation"))
            raise ClaudeSubError("Claude foreground process exited before hook-confirmed completion")

        try:
          event = await asyncio.wait_for(bridge.events.get(), timeout=_POLL_SECONDS)
        except TimeoutError:
          continue
        _emit(event)
    except Exception as error:
      if launch_attempted and not terminated and not completed:
        try:
          await _terminate_foreground(session_id)
        except Exception as termination_error:
          raise ClaudeSubError(
              f"{error}; failed to terminate the Claude foreground process: {termination_error}") from error
      raise
    finally:
      await bridge.stop()


async def _run(args: ClaudeSubArgs) -> None:
  await _check_cli_capabilities()
  stop_event = asyncio.Event()
  loop = asyncio.get_running_loop()
  for sig in (signal.SIGINT, signal.SIGTERM):
    loop.add_signal_handler(sig, stop_event.set)
  await _stream_turn(args, stop_event)


def main(argv: list[str] | None = None) -> int:
  try:
    args = parse_argv(sys.argv[1:] if argv is None else argv)
    args = ClaudeSubArgs(**{**args.__dict__, "prompt": sys.stdin.read()})
    asyncio.run(_run(args))
    return 0
  except Exception as error:
    message = f"claude-sub: {error}"
    try:
      _emit(_error_event(message))
    finally:
      print(message, file=sys.stderr)
    return 1


if __name__ == "__main__":
  raise SystemExit(main())
