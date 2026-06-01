"""Host-global web terminal backed by one tmux session."""

from __future__ import annotations

import asyncio
import base64
import json
from pathlib import Path

import structlog
from fastapi import WebSocket, WebSocketDisconnect

from src.agents.backends.pty_common import (
    PTY_EXIT,
    PTY_INPUT,
    PTY_RESIZE,
    PtyAttachment,
    _HISTORY_LIMIT,
    _INITIAL_COLS,
    _INITIAL_ROWS,
    _WS_RECV_TIMEOUT,
    _pump_pty_to_ws,
    _run_tmux,
    tmux_session_name,
)

log = structlog.get_logger()

_TERMINAL_SESSION_ID = "terminal"
_TERMINAL_TMUX_NAME = tmux_session_name(_TERMINAL_SESSION_ID)
_ensure_lock = asyncio.Lock()


async def _terminal_session_exists() -> bool:
  rc, _ = await _run_tmux("has-session", "-t", _TERMINAL_TMUX_NAME)
  return rc == 0


async def ensure_terminal_session() -> None:
  """Idempotently create the host-global terminal tmux session."""
  async with _ensure_lock:
    if await _terminal_session_exists():
      return
    home = Path.home()
    rc, stderr = await _run_tmux(
        "new-session",
        "-d",
        "-s",
        _TERMINAL_TMUX_NAME,
        "-x",
        str(_INITIAL_COLS),
        "-y",
        str(_INITIAL_ROWS),
        "-c",
        str(home),
        "bash",
        "-l",
    )
    if rc != 0:
      raise RuntimeError(f"tmux new-session failed for {_TERMINAL_TMUX_NAME}: {stderr.strip()}")
    await _run_tmux("set-option", "-t", _TERMINAL_TMUX_NAME, "history-limit", str(_HISTORY_LIMIT))
    log.info("terminal_tmux_session_created", name=_TERMINAL_TMUX_NAME, cwd=str(home))


async def run_terminal_attachment(websocket: WebSocket) -> None:
  """Attach this WebSocket to the host-global terminal tmux session."""
  try:
    await ensure_terminal_session()
  except Exception as e:  # noqa: BLE001 — surface to client
    log.exception("terminal_ensure_session_failed")
    try:
      await websocket.send_json({"type": PTY_EXIT, "error": str(e)})
    except Exception as send_error:  # noqa: BLE001
      log.debug("terminal_ensure_error_send_failed", error=str(send_error))
    return

  attachment = PtyAttachment(_TERMINAL_SESSION_ID)
  try:
    attachment.spawn()
  except Exception as e:  # noqa: BLE001
    log.exception("terminal_pty_spawn_failed")
    try:
      await websocket.send_json({"type": PTY_EXIT, "error": str(e)})
    except Exception as send_error:  # noqa: BLE001
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
        except Exception as e:  # noqa: BLE001
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
        except Exception as e:  # noqa: BLE001 — malformed input from client
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
    except Exception as e:  # noqa: BLE001
      log.debug("terminal_pump_task_exit", error=str(e))
    attachment.close()
