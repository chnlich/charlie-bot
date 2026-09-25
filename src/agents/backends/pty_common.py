"""Shared tmux/PTY helpers for browser-backed interactive terminals."""

from __future__ import annotations

import asyncio
import base64
import contextlib
import fcntl
import json
import os
import shutil
import signal
import struct
import tempfile
import termios
from typing import TYPE_CHECKING

from src.core.constants import SESSION_ID_ENV_VAR
from src.core.log_once import LazyStructlogLogger
from src.core.timeouts import PTY_WS_RECV_TIMEOUT

log = LazyStructlogLogger()

PTY_INPUT = "pty_input"
PTY_OUTPUT = "pty_output"
PTY_RESIZE = "pty_resize"
PTY_EXIT = "pty_exit"

_TMUX_SOCKET = "charliebot"
_INITIAL_COLS = 80
_INITIAL_ROWS = 24
_HISTORY_LIMIT = 50000
_PTY_READ_CHUNK = 4096
# The browser end of every PTY is xterm.js; tmux only emits OSC 52 to a client whose
# terminfo advertises `Ms`, which screen-256color does not.
_PTY_CLIENT_TERM = "xterm-256color"

# fastapi rides every claude-sub launch (the worker binary imports this module for
# the tmux helpers); the WS-facing relay below is the only fastapi consumer, so its
# imports stay inside that function and the WebSocket type rides TYPE_CHECKING.
if TYPE_CHECKING:
  from fastapi import WebSocket


def _tmux_binary() -> str:
  """Resolve the tmux binary path, raising a clear error if missing."""
  path = shutil.which("tmux")
  if not path:
    raise RuntimeError("tmux binary not found on PATH — install tmux for tui-cli backend")
  return path


def tmux_session_name(session_id: str) -> str:
  """Return the tmux session name for a CharlieBot session id."""
  return f"charliebot-{session_id}"


def _tmux_client_env() -> dict[str, str]:
  env = {**os.environ}
  env.pop(SESSION_ID_ENV_VAR, None)
  return env


def _tmux_pty_env() -> dict[str, str]:
  """Env for the browser-facing `tmux attach` client; never inherits the server's TERM.

  The other end of this PTY is xterm.js. Inheriting the server's TERM (screen-256color
  whenever the server itself was started inside tmux) hides the `Ms` terminfo capability
  that tmux needs before it will push a copy out as OSC 52.
  """
  return {**_tmux_client_env(), "TERM": _PTY_CLIENT_TERM}


async def _run_tmux(*args: str, capture: bool = False) -> tuple[int, str]:
  """Run a tmux command on the isolated socket. Returns (exit_code, text).

  The text is stderr; with capture=True a zero exit swaps in stdout.
  """
  tmux = _tmux_binary()
  env = _tmux_client_env()
  env.pop("TMUX", None)
  cmd = [tmux, "-L", _TMUX_SOCKET, *args]
  # tmux new-session can fork a server daemon that inherits stderr; under uvloop,
  # communicate() waits forever for PIPE EOF, so capture stderr in a regular file.
  with tempfile.TemporaryFile() as stderr_f:
    proc = await asyncio.create_subprocess_exec(
        *cmd,
        stdin=asyncio.subprocess.DEVNULL,
        stdout=asyncio.subprocess.PIPE if capture else asyncio.subprocess.DEVNULL,
        stderr=stderr_f,
        env=env,
    )
    stdout, _ = await proc.communicate()
    stderr_f.seek(0)
    stderr_b = stderr_f.read()
  stderr = stderr_b.decode("utf-8", errors="replace").strip() if stderr_b else ""
  out = stdout.decode("utf-8", errors="replace") if stdout else ""
  rc = proc.returncode or 0
  return rc, out if capture and rc == 0 else stderr or out


async def _start_tmux_session(name: str, cwd: str, env_args: list[str], command_argv: list[str]) -> None:
  """Start a detached tmux session named *name* running *command_argv* in *cwd*.

  *env_args* holds ``-e KEY=VALUE`` pairs for new-session. The window starts
  at the shared initial geometry, and the shared scrollback limit is set
  after creation. Raises RuntimeError when the new-session call fails.
  """
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
      cwd,
      # tmux reads every option up to the command argv; an "-e" pair that
      # follows the command becomes part of it.
      *env_args,
      *command_argv,
  )
  if rc != 0:
    raise RuntimeError(f"tmux new-session failed for {name}: {stderr.strip()}")
  await _run_tmux("set-option", "-t", name, "history-limit", str(_HISTORY_LIMIT))


async def tmux_pane_pid(session_id: str) -> int | None:
  """The first pane's process pid of this session's tmux session, or None.

  The TUI Run's process identity: the claude CLI lives in the pane, so its pid
  (pinned with its /proc start marker by the caller) is what the Run records
  and the caller-identity checks verify — the same Run/pid owners as a
  headless launch.
  """
  rc, out = await _run_tmux("list-panes", "-t", tmux_session_name(session_id), "-F", "#{pane_pid}", capture=True)
  if rc != 0 or not out.strip():
    return None
  try:
    return int(out.strip().split("\n")[0])
  except ValueError as e:
    raise RuntimeError(f"tmux pane pid unparsable for {session_id}: {out!r}") from e


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
    # pty drags tty+termios into every pty_common importer's import; only this
    # server-side attachment forks one, so the import rides the call (the M108
    # launch floor in docs/perf_baseline.md prices pty_common on every
    # claude-sub worker launch).
    import pty

    tmux = _tmux_binary()
    name = tmux_session_name(self.session_id)
    pid, fd = pty.fork()
    if pid == 0:
      # Child process — exec tmux attach.
      try:
        os.execve(tmux, [tmux, "-L", _TMUX_SOCKET, "attach", "-t", name], _tmux_pty_env())
      except Exception as e:  # last-ditch report before _exit
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
      with contextlib.suppress(OSError):
        os.close(self.fd)
      self.fd = -1
    if self.pid > 0:
      with contextlib.suppress(ProcessLookupError):
        os.kill(self.pid, signal.SIGTERM)
      with contextlib.suppress(ChildProcessError):
        os.waitpid(self.pid, os.WNOHANG)


async def _pump_pty_to_ws(attachment: PtyAttachment, websocket: WebSocket) -> None:
  """Forward bytes from the PTY to the WebSocket as base64 `pty_output` events."""
  loop = asyncio.get_running_loop()
  q: asyncio.Queue = asyncio.Queue()
  _eof = object()
  fd = attachment.fd

  def _on_readable() -> None:
    try:
      chunk = os.read(fd, _PTY_READ_CHUNK)
    except BlockingIOError:
      return
    except OSError as e:
      log.debug("tui_pty_read_oserror", session_id=attachment.session_id, error=str(e))
      q.put_nowait(_eof)
      return
    if not chunk:
      q.put_nowait(_eof)
      return
    q.put_nowait(chunk)

  loop.add_reader(fd, _on_readable)
  try:
    while True:
      item = await q.get()
      if item is _eof:
        break
      try:
        await websocket.send_json({
            "type": PTY_OUTPUT,
            "data": base64.b64encode(item).decode("ascii"),
        })
      except Exception as e:  # WS already closed/broken
        log.debug("tui_pty_ws_send_failed", session_id=attachment.session_id, error=str(e))
        return
  finally:
    try:
      loop.remove_reader(fd)
    except Exception as e:  # fd already closed
      log.debug("tui_pty_remove_reader_failed", session_id=attachment.session_id, error=str(e))
  try:
    await websocket.send_json({"type": PTY_EXIT})
  except Exception as e:
    log.debug("tui_pty_exit_send_failed", session_id=attachment.session_id, error=str(e))


async def _run_pty_relay(websocket: WebSocket, attachment: PtyAttachment, *, pump_name: str) -> None:
  """Run the bidirectional PTY↔WebSocket relay until the WebSocket drops.

  Starts the PTY→WS pump, forwards browser `pty_input`/`pty_resize` messages to
  the PTY, then on exit cancels the pump and closes the attachment.
  """
  from fastapi import WebSocketDisconnect

  pump_task = asyncio.create_task(
      _pump_pty_to_ws(attachment, websocket),
      name=pump_name,
  )
  try:
    while True:
      try:
        raw = await asyncio.wait_for(websocket.receive_text(), timeout=PTY_WS_RECV_TIMEOUT)
      except TimeoutError:
        try:
          await websocket.send_json({"type": "ping"})
        except Exception as e:
          log.debug("tui_pty_ping_send_failed", session_id=attachment.session_id, error=str(e))
          break
        continue
      except WebSocketDisconnect:
        break
      try:
        msg = json.loads(raw)
      except json.JSONDecodeError as e:
        log.debug("tui_pty_ws_json_decode_failed", session_id=attachment.session_id, error=str(e))
        continue
      t = msg.get("type")
      if t == PTY_INPUT:
        payload = msg.get("data") or ""
        try:
          chunk = base64.b64decode(payload, validate=False)
        except Exception as e:  # malformed input from client
          log.debug("tui_pty_input_decode_failed", session_id=attachment.session_id, error=str(e))
          continue
        attachment.write(chunk)
      elif t == PTY_RESIZE:
        try:
          cols = int(msg.get("cols") or _INITIAL_COLS)
          rows = int(msg.get("rows") or _INITIAL_ROWS)
        except (TypeError, ValueError) as e:
          log.debug("tui_pty_resize_parse_failed", session_id=attachment.session_id, error=str(e))
          continue
        attachment.resize(cols, rows)
      # Other types (legacy `cursor`, future events) are ignored.
  finally:
    pump_task.cancel()
    try:
      await pump_task
    except asyncio.CancelledError:
      pass
    except Exception as e:
      log.debug("tui_pump_task_exit", session_id=attachment.session_id, error=str(e))
    attachment.close()
