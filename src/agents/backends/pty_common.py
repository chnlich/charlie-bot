"""Shared tmux/PTY helpers for browser-backed interactive terminals."""

from __future__ import annotations

import asyncio
import base64
import fcntl
import os
import pty
import shutil
import signal
import struct
import termios

import structlog
from fastapi import WebSocket

log = structlog.get_logger()

PTY_INPUT = "pty_input"
PTY_OUTPUT = "pty_output"
PTY_RESIZE = "pty_resize"
PTY_EXIT = "pty_exit"

_TMUX_SOCKET = "charliebot"
_INITIAL_COLS = 80
_INITIAL_ROWS = 24
_HISTORY_LIMIT = 50000
_PTY_READ_CHUNK = 4096
_WS_RECV_TIMEOUT = 30.0


def _tmux_binary() -> str:
  """Resolve the tmux binary path, raising a clear error if missing."""
  path = shutil.which("tmux")
  if not path:
    raise RuntimeError("tmux binary not found on PATH — install tmux for tui-cli backend")
  return path


def tmux_session_name(session_id: str) -> str:
  """Return the tmux session name for a CharlieBot session id."""
  return f"charliebot-{session_id}"


async def _run_tmux(*args: str, check: bool = False) -> tuple[int, str]:
  """Run a tmux command on the isolated socket. Returns (exit_code, stderr)."""
  tmux = _tmux_binary()
  cmd = [tmux, "-L", _TMUX_SOCKET, *args]
  proc = await asyncio.create_subprocess_exec(
      *cmd,
      stdout=asyncio.subprocess.DEVNULL,
      stderr=asyncio.subprocess.PIPE,
  )
  _, stderr_b = await proc.communicate()
  stderr = stderr_b.decode("utf-8", errors="replace") if stderr_b else ""
  rc = proc.returncode or 0
  if check and rc != 0:
    raise RuntimeError(f"tmux {' '.join(args)} failed (rc={rc}): {stderr.strip()}")
  return rc, stderr


async def tmux_session_exists(session_id: str) -> bool:
  """Return True if the tmux session for *session_id* exists on the charliebot socket."""
  rc, _ = await _run_tmux("has-session", "-t", tmux_session_name(session_id))
  return rc == 0


async def kill_tmux_session(session_id: str) -> None:
  """Best-effort kill of the tmux session for *session_id*."""
  name = tmux_session_name(session_id)
  rc, stderr = await _run_tmux("kill-session", "-t", name)
  if rc == 0:
    log.info("tui_tmux_session_killed", session_id=session_id, name=name)
  else:
    log.debug("tui_tmux_session_kill_noop", session_id=session_id, name=name, stderr=stderr.strip())


def _set_winsize(fd: int, rows: int, cols: int) -> None:
  size = struct.pack("HHHH", rows, cols, 0, 0)
  fcntl.ioctl(fd, termios.TIOCSWINSZ, size)


class PtyAttachment:
  """A per-WS-connection PTY wrapping `tmux -L charliebot attach -t charliebot-{id}`."""

  def __init__(self, session_id: str) -> None:
    self.session_id = session_id
    self.pid: int = -1
    self.fd: int = -1
    self._closed = False

  def spawn(self) -> None:
    """Fork a PTY child that execs `tmux attach` for this session."""
    tmux = _tmux_binary()
    name = tmux_session_name(self.session_id)
    pid, fd = pty.fork()
    if pid == 0:
      # Child process — exec tmux attach.
      os.environ.setdefault("TERM", "xterm-256color")
      try:
        os.execv(tmux, [tmux, "-L", _TMUX_SOCKET, "attach", "-t", name])
      except Exception as e:  # noqa: BLE001 — last-ditch report before _exit
        os.write(2, f"tmux exec failed: {e}\n".encode())
        os._exit(1)
    self.pid = pid
    self.fd = fd
    _set_winsize(self.fd, _INITIAL_ROWS, _INITIAL_COLS)
    fl = fcntl.fcntl(self.fd, fcntl.F_GETFL)
    fcntl.fcntl(self.fd, fcntl.F_SETFL, fl | os.O_NONBLOCK)
    log.info("tui_pty_spawned", session_id=self.session_id, pid=self.pid, fd=self.fd)

  def write(self, data: bytes) -> None:
    if self._closed or self.fd < 0 or not data:
      return
    try:
      os.write(self.fd, data)
    except OSError as e:
      log.warning("tui_pty_write_failed", session_id=self.session_id, error=str(e))

  def resize(self, cols: int, rows: int) -> None:
    if self._closed or self.fd < 0:
      return
    try:
      _set_winsize(self.fd, max(1, rows), max(1, cols))
    except OSError as e:
      log.warning("tui_pty_resize_failed", session_id=self.session_id, error=str(e))

  def close(self) -> None:
    if self._closed:
      return
    self._closed = True
    if self.fd >= 0:
      try:
        os.close(self.fd)
      except OSError:
        pass
      self.fd = -1
    if self.pid > 0:
      try:
        os.kill(self.pid, signal.SIGTERM)
      except ProcessLookupError:
        pass
      try:
        os.waitpid(self.pid, os.WNOHANG)
      except ChildProcessError:
        pass


async def _pump_pty_to_ws(attachment: PtyAttachment, websocket: WebSocket) -> None:
  """Forward bytes from the PTY to the WebSocket as base64 `pty_output` events."""
  loop = asyncio.get_running_loop()
  q: asyncio.Queue = asyncio.Queue()
  _EOF = object()
  fd = attachment.fd

  def _on_readable() -> None:
    try:
      chunk = os.read(fd, _PTY_READ_CHUNK)
    except BlockingIOError:
      return
    except OSError as e:
      log.debug("tui_pty_read_oserror", session_id=attachment.session_id, error=str(e))
      q.put_nowait(_EOF)
      return
    if not chunk:
      q.put_nowait(_EOF)
      return
    q.put_nowait(chunk)

  loop.add_reader(fd, _on_readable)
  try:
    while True:
      item = await q.get()
      if item is _EOF:
        break
      try:
        await websocket.send_json({
            "type": PTY_OUTPUT,
            "data": base64.b64encode(item).decode("ascii"),
        })
      except Exception as e:  # noqa: BLE001 — WS already closed/broken
        log.debug("tui_pty_ws_send_failed", session_id=attachment.session_id, error=str(e))
        return
  finally:
    try:
      loop.remove_reader(fd)
    except Exception as e:  # noqa: BLE001 — fd already closed
      log.debug("tui_pty_remove_reader_failed", session_id=attachment.session_id, error=str(e))
  try:
    await websocket.send_json({"type": PTY_EXIT})
  except Exception as e:  # noqa: BLE001
    log.debug("tui_pty_exit_send_failed", session_id=attachment.session_id, error=str(e))
