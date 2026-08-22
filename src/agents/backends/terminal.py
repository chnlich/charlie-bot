"""Per-profile web terminal backed by one tmux session."""

from __future__ import annotations

import asyncio
import base64
import hashlib
import json
import os
from pathlib import Path

import structlog
from fastapi import WebSocket, WebSocketDisconnect

from src.agents.backends.pty_common import (
  _HISTORY_LIMIT,
  _INITIAL_COLS,
  _INITIAL_ROWS,
  _WS_RECV_TIMEOUT,
  PTY_EXIT,
  PTY_INPUT,
  PTY_RESIZE,
  PtyAttachment,
  _pump_pty_to_ws,
  _run_tmux,
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
    rc, stderr = await _run_tmux(
        "new-session",
        "-d",
        "-s",
        name,
        "-x",
        str(_INITIAL_COLS),
        "-y",
        str(_INITIAL_ROWS),
        "-c",
        str(home),
        *env_args,
        "bash",
        "-l",
    )
    if rc != 0:
      raise RuntimeError(f"tmux new-session failed for {name}: {stderr.strip()}")
    await _run_tmux("set-option", "-t", name, "history-limit", str(_HISTORY_LIMIT))
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

  pump_task = asyncio.create_task(
      _pump_pty_to_ws(attachment, websocket),
      name="terminal-pump",
  )
  try:
    while True:
      try:
        raw = await asyncio.wait_for(websocket.receive_text(), timeout=_WS_RECV_TIMEOUT)
      except asyncio.TimeoutError:
        try:
          await websocket.send_json({"type": "ping"})
        except Exception as e:
          log.debug("terminal_ping_send_failed", error=str(e))
          break
        continue
      except WebSocketDisconnect:
        break
      try:
        msg = json.loads(raw)
      except json.JSONDecodeError as e:
        log.debug("terminal_ws_json_decode_failed", error=str(e))
        continue
      t = msg.get("type")
      if t == PTY_INPUT:
        payload = msg.get("data") or ""
        try:
          chunk = base64.b64decode(payload, validate=False)
        except Exception as e:  # malformed input from client
          log.debug("terminal_pty_input_decode_failed", error=str(e))
          continue
        attachment.write(chunk)
      elif t == PTY_RESIZE:
        try:
          cols = int(msg.get("cols") or 80)
          rows = int(msg.get("rows") or 24)
        except (TypeError, ValueError) as e:
          log.debug("terminal_pty_resize_parse_failed", error=str(e))
          continue
        attachment.resize(cols, rows)
  finally:
    pump_task.cancel()
    try:
      await pump_task
    except asyncio.CancelledError:
      pass
    except Exception as e:
      log.debug("terminal_pump_task_exit", error=str(e))
    attachment.close()
