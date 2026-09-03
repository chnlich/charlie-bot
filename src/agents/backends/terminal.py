"""Per-profile web terminal backed by one tmux session."""

from __future__ import annotations

import asyncio
import hashlib
import os
from pathlib import Path

import structlog
from fastapi import WebSocket

from src.agents.backends.pty_common import (
    PTY_EXIT,
    PtyAttachment,
    _run_pty_relay,
    _run_tmux,
    _start_tmux_session,
    tmux_session_name,
)
from src.core.config import (
    CHARLIEBOT_HOME_ENV,
    charliebot_home_dir,
    default_charliebot_home,
)

log = structlog.get_logger()

_TERMINAL_SESSION_ID = "terminal"
_ensure_lock = asyncio.Lock()


def terminal_session_id() -> str:
  """The terminal's session id for this profile.

  The tmux server is shared by every profile on the host, so the name is what keeps
  two profiles from attaching to the same shell. The default home keeps the
  historical id; any other home appends a digest of its path. Attachment derives the
  tmux name from this id (:class:`PtyAttachment`), so the suffix has to live here.
  """
  home = charliebot_home_dir()
  if home == default_charliebot_home():
    return _TERMINAL_SESSION_ID
  digest = hashlib.sha256(str(home).encode("utf-8")).hexdigest()[:8]
  return f"{_TERMINAL_SESSION_ID}-{digest}"


def terminal_tmux_name() -> str:
  """The tmux session name of this profile's terminal."""
  return tmux_session_name(terminal_session_id())


async def _terminal_session_exists(name: str) -> bool:
  rc, _ = await _run_tmux("has-session", "-t", name)
  return rc == 0


async def ensure_terminal_session() -> None:
  """Idempotently create this profile's terminal tmux session."""
  async with _ensure_lock:
    name = terminal_tmux_name()
    if await _terminal_session_exists(name):
      return
    home = Path.home()
    # A pane inherits the tmux *server's* environment, not this process's, and that
    # server may have been started by another profile. Pass the profile explicitly so
    # a charliebot command typed in this terminal acts on the instance that opened it.
    env_args: list[str] = []
    if os.environ.get(CHARLIEBOT_HOME_ENV, "").strip():
      env_args = ["-e", f"{CHARLIEBOT_HOME_ENV}={charliebot_home_dir()}"]
    await _start_tmux_session(name, str(home), env_args, ["bash", "-l"])
    log.info("terminal_tmux_session_created", name=name, cwd=str(home))


async def run_terminal_attachment(websocket: WebSocket) -> None:
  """Attach this WebSocket to this profile's terminal tmux session."""
  try:
    await ensure_terminal_session()
  except Exception as e:  # surface to client
    log.exception("terminal_ensure_session_failed")
    try:
      await websocket.send_json({"type": PTY_EXIT, "error": str(e)})
    except Exception as send_error:
      log.debug("terminal_ensure_error_send_failed", error=str(send_error))
    return

  attachment = PtyAttachment(terminal_session_id())
  try:
    attachment.spawn()
  except Exception as e:
    log.exception("terminal_pty_spawn_failed")
    try:
      await websocket.send_json({"type": PTY_EXIT, "error": str(e)})
    except Exception as send_error:
      log.debug("terminal_spawn_error_send_failed", error=str(send_error))
    return

  await _run_pty_relay(websocket, attachment, pump_name="terminal-pump")
