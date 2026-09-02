"""TUI backend — runs the `claude` CLI inside an isolated tmux session.

Each CharlieBot session_id maps to one tmux session named ``charliebot-{id}``
under the ``charliebot`` tmux socket so it never collides with the user's
normal tmux sessions. Per WebSocket connection, a ``tmux attach`` PTY is
spawned and bytes are forwarded between the PTY and the browser's xterm.js
terminal. Multiple browsers attaching the same session is tmux-native.
"""

from __future__ import annotations

import contextlib
import json
import os
import time
from pathlib import Path
from typing import Any

import structlog
from fastapi import WebSocket

from src.agents.backends.base import SKIP_PERMISSIONS_FLAG, SKIP_PERMISSIONS_SETTINGS
from src.agents.backends.pty_common import (
  PTY_EXIT,
  PtyAttachment,
  _run_pty_relay,
  _start_tmux_session,
  _tmux_binary,
  kill_tmux_session,  # noqa: F401  # re-export: imported from this module by src/api/sessions.py + src/core/sessions.py and monkeypatched here by tests
  tmux_session_exists,
  tmux_session_name,
)

log = structlog.get_logger()

_CLAUDE_TUI_SETTINGS = json.dumps(SKIP_PERMISSIONS_SETTINGS, separators=(",", ":"))
_BUSY_THRESHOLD_SECONDS = 3.0


def _find_existing_claude_jsonl(session_id: str) -> Path | None:
  """Glob ~/.claude/projects/*/<session_id>.jsonl and return first match (or None)."""
  matches = list(Path.home().glob(f".claude/projects/*/{session_id}.jsonl"))
  return matches[0] if matches else None


def _claude_jsonl_busy(session_id: str, threshold_seconds: float = _BUSY_THRESHOLD_SECONDS) -> bool:
  """Return True if claude's jsonl for this session was written to within threshold_seconds.
  Uses the same glob path as _find_existing_claude_jsonl. Returns False if no jsonl found."""
  jsonl = _find_existing_claude_jsonl(session_id)
  if jsonl is None:
    return False
  mtime = jsonl.stat().st_mtime
  return (time.time() - mtime) < threshold_seconds


def _build_claude_argv(
    session_id: str,
    resume: bool,
    *,
    model: str | None = None,
    effort: str | None = None,
    disallowed_tools: list[str] | None = None,
) -> list[str]:
  session_arg = "--resume" if resume else "--session-id"
  argv = [
      "claude",
      "--settings",
      _CLAUDE_TUI_SETTINGS,
      SKIP_PERMISSIONS_FLAG,
      session_arg,
      session_id,
  ]
  if model:
    argv.extend(["--model", model])
  if effort:
    argv.extend(["--effort", effort])
  # Collapse every incoming entry into one comma-joined value: the launched `claude`
  # reliably honors a single --disallowed-tools flag, not repeated ones.
  if disallowed_tools:
    argv.extend(["--disallowed-tools", ",".join(disallowed_tools)])
  return argv


def _claude_config_path() -> Path:
  config_dir = os.environ.get("CLAUDE_CONFIG_DIR")
  if config_dir:
    return Path(config_dir) / ".claude.json"
  return Path.home() / ".claude.json"


def mark_project_trusted(config: dict[str, Any], project_path: str) -> bool:
  """Mark *project_path* trusted in a ``.claude.json``-shaped *config* dict.

  *config* maps project path strings under ``"projects"`` to per-project entries.
  The entry gains ``hasTrustDialogAccepted: true`` and, when absent,
  ``projectOnboardingSeenCount: 1``; Claude Code skips its interactive workspace-trust
  dialog only when both are present. Returns True when the entry changed, so the caller
  writes the file only then.
  """
  project = config.setdefault("projects", {}).setdefault(project_path, {})
  changed = project.get("hasTrustDialogAccepted") is not True or "projectOnboardingSeenCount" not in project
  project["hasTrustDialogAccepted"] = True
  project.setdefault("projectOnboardingSeenCount", 1)
  return changed


def _ensure_claude_project_trusted(working_dir: Path) -> None:
  """Mark CharlieBot's generated Claude TUI cwd trusted before interactive startup."""
  project_path = str(working_dir.resolve())
  config_path = _claude_config_path()
  if config_path.exists():
    config = json.loads(config_path.read_text(encoding="utf-8"))
  else:
    config = {}
  if not mark_project_trusted(config, project_path):
    return
  config_path.parent.mkdir(parents=True, exist_ok=True)
  config_path.write_text(json.dumps(config, indent=2) + "\n", encoding="utf-8")
  log.info("tui_claude_project_trusted", path=project_path, config_path=str(config_path))


async def ensure_tmux_session(
    session_id: str,
    working_dir: Path,
    *,
    model: str | None = None,
    effort: str | None = None,
    disallowed_tools: list[str] | None = None,
    inject_env: dict[str, str] | None = None,
) -> None:
  """Idempotently create the tmux session running Claude TUI in *working_dir*."""
  name = tmux_session_name(session_id)
  working_dir.mkdir(parents=True, exist_ok=True)
  _ensure_claude_project_trusted(working_dir)
  if await tmux_session_exists(session_id):
    return
  resume = _find_existing_claude_jsonl(session_id) is not None
  command_args = _build_claude_argv(
      session_id,
      resume,
      model=model,
      effort=effort,
      disallowed_tools=disallowed_tools,
  )
  tmux_env_args: list[str] = []
  if inject_env is not None:
    for key, value in inject_env.items():
      tmux_env_args.extend(["-e", f"{key}={value}"])
  log.info("tui_claude_invocation", mode="resume" if resume else "fresh", session_id=session_id)
  await _start_tmux_session(name, str(working_dir), tmux_env_args, command_args)
  log.info("tui_tmux_session_created", session_id=session_id, name=name, cwd=str(working_dir))


class TuiBackend:
  """Lightweight backend descriptor for tui-cli sessions.

  Does not inherit AgentBackend — the SDK streaming template is the wrong
  shape for a long-lived interactive PTY. The actual lifecycle work
  (tmux + PTY) lives in module-level helpers invoked from the WebSocket
  handler and session manager.
  """

  type = "tui-cli"

  def __init__(self, **_kwargs: object) -> None:
    # Validate tmux is available at construction time so config errors fail fast.
    _tmux_binary()


async def run_tui_attachment(websocket: WebSocket, session_id: str, sessions_dir: Path) -> None:
  """Per-WS PTY loop: spawn `tmux attach`, pump bytes, handle pty_input/pty_resize.

  Returns when the WebSocket disconnects or the PTY exits. Called from the
  session WebSocket handler after subscription + catchup.
  """
  try:
    await ensure_tmux_session(session_id, sessions_dir / session_id)
  except Exception as e:  # surface to client
    log.exception("tui_ensure_session_failed", session_id=session_id)
    with contextlib.suppress(Exception):
      await websocket.send_json({"type": PTY_EXIT, "error": str(e)})
    return

  attachment = PtyAttachment(session_id)
  try:
    attachment.spawn()
  except Exception as e:
    log.exception("tui_pty_spawn_failed", session_id=session_id)
    with contextlib.suppress(Exception):
      await websocket.send_json({"type": PTY_EXIT, "error": str(e)})
    return

  try:
    await _run_pty_relay(websocket, attachment, pump_name=f"tui-pump-{session_id[:8]}")
  finally:
    try:
      from src.api.deps import get_session_manager
      from src.core.autonamer import maybe_auto_name_from_claude_ai_title

      session_mgr = get_session_manager()
      meta = await session_mgr.get_session(session_id)
      if meta is None:
        log.warning("tui_autoname_session_missing", session_id=session_id)
      else:
        await maybe_auto_name_from_claude_ai_title(meta, session_mgr)
    except Exception as e:  # autonaming must not break PTY cleanup
      log.warning("tui_autoname_failed", session_id=session_id, error=str(e), exc_info=True)
