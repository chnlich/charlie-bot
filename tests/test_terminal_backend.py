import asyncio
import base64
import json
import os
import select
import shutil
import subprocess
import time
import uuid
from pathlib import Path

import pytest
from fastapi import WebSocketDisconnect

from src.agents.backends import pty_common, terminal, tui
from src.agents.backends.pty_common import PTY_INPUT, PTY_RESIZE


def _b64(data: bytes) -> str:
  return base64.b64encode(data).decode("ascii")


class _ScriptedWebSocket:
  def __init__(self, messages: list[dict]) -> None:
    self._messages = [json.dumps(message) for message in messages]
    self.sent: list[dict] = []

  async def receive_text(self) -> str:
    if self._messages:
      return self._messages.pop(0)
    raise WebSocketDisconnect()

  async def send_json(self, payload: dict) -> None:
    self.sent.append(payload)


class _AcceptingWebSocket:
  def __init__(self) -> None:
    self.accepted = False

  async def accept(self) -> None:
    self.accepted = True


class _FakeAttachment:
  instances: list["_FakeAttachment"] = []

  def __init__(self, session_id: str) -> None:
    self.session_id = session_id
    self.spawned = False
    self.closed = False
    self.writes: list[bytes] = []
    self.resizes: list[tuple[int, int]] = []
    _FakeAttachment.instances.append(self)

  def spawn(self) -> None:
    self.spawned = True

  def write(self, data: bytes) -> None:
    self.writes.append(data)

  def resize(self, cols: int, rows: int) -> None:
    self.resizes.append((cols, rows))

  def close(self) -> None:
    self.closed = True


@pytest.mark.asyncio
async def test_run_tmux_strips_session_env(monkeypatch: pytest.MonkeyPatch) -> None:
  captured: dict[str, dict[str, str]] = {}
  monkeypatch.setenv("CHARLIEBOT_SESSION_ID", "stale-session")
  monkeypatch.setattr(pty_common, "_tmux_binary", lambda: "/usr/bin/tmux")

  class FakeProcess:
    returncode = 0

    async def wait(self) -> None:
      return None

  async def fake_create_subprocess_exec(*args, **kwargs):
    captured["env"] = kwargs["env"]
    return FakeProcess()

  monkeypatch.setattr(pty_common.asyncio, "create_subprocess_exec", fake_create_subprocess_exec)

  rc, stderr = await pty_common._run_tmux("has-session", "-t", "charliebot-session")

  assert rc == 0
  assert stderr == ""
  assert "CHARLIEBOT_SESSION_ID" not in captured["env"]


def test_run_tmux_new_session_returns_under_uvloop(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  uvloop = pytest.importorskip("uvloop")
  tmux = shutil.which("tmux")
  if tmux is None:
    pytest.skip("tmux binary not available")

  socket_name = f"charliebot-test-{os.getpid()}-{uuid.uuid4().hex}"
  session_name = f"charliebot-test-{uuid.uuid4().hex}"
  monkeypatch.setattr(pty_common, "_TMUX_SOCKET", socket_name)

  async def scenario() -> None:
    try:
      missing_rc, missing_stderr = await pty_common._run_tmux("has-session", "-t", session_name)
      assert missing_rc != 0
      assert missing_stderr.strip()

      rc, stderr = await asyncio.wait_for(
          pty_common._run_tmux(
              "new-session",
              "-d",
              "-s",
              session_name,
              "-x",
              "80",
              "-y",
              "24",
              "-c",
              str(tmp_path),
              "sleep 60",
          ),
          timeout=5.0,
      )
      assert rc == 0, stderr

      rc, stderr = await pty_common._run_tmux("set-option", "-t", session_name, "history-limit", "50000")
      assert rc == 0, stderr

      rc, stderr = await pty_common._run_tmux("kill-session", "-t", session_name)
      assert rc == 0, stderr
    finally:
      cleanup = subprocess.run(
          [tmux, "-L", socket_name, "kill-server"],
          stdout=subprocess.DEVNULL,
          stderr=subprocess.PIPE,
          text=True,
          check=False,
      )
      assert cleanup.returncode in (0, 1), cleanup.stderr

  with asyncio.Runner(loop_factory=uvloop.new_event_loop) as runner:
    runner.run(scenario())


@pytest.mark.asyncio
async def test_ensure_terminal_session_starts_global_login_shell(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  home_dir = tmp_path / "home"
  home_dir.mkdir()
  calls = []
  monkeypatch.setattr(terminal.Path, "home", staticmethod(lambda: home_dir))

  async def fake_run_tmux(*args: str, check: bool = False) -> tuple[int, str]:
    calls.append(args)
    if args[0] == "has-session":
      return 1, ""
    return 0, ""

  monkeypatch.setattr(terminal, "_run_tmux", fake_run_tmux)

  await terminal.ensure_terminal_session()

  assert calls[0] == ("has-session", "-t", "charliebot-terminal")
  assert (
      "new-session",
      "-d",
      "-s",
      "charliebot-terminal",
      "-x",
      "80",
      "-y",
      "24",
      "-c",
      str(home_dir),
      "bash",
      "-l",
  ) in calls
  assert ("set-option", "-t", "charliebot-terminal", "history-limit", "50000") in calls


@pytest.mark.asyncio
async def test_ensure_terminal_session_reuses_existing_tmux_session(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  calls = []

  async def fake_run_tmux(*args: str, check: bool = False) -> tuple[int, str]:
    calls.append(args)
    return 0, ""

  monkeypatch.setattr(terminal, "_run_tmux", fake_run_tmux)

  await terminal.ensure_terminal_session()

  assert calls == [("has-session", "-t", "charliebot-terminal")]


@pytest.mark.asyncio
async def test_run_terminal_attachment_attaches_and_handles_input(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  ensured = []
  _FakeAttachment.instances = []
  ws = _ScriptedWebSocket([
      {"type": PTY_INPUT, "data": _b64(b"pwd\n")},
      {"type": PTY_RESIZE, "cols": 100, "rows": 30},
  ])

  async def fake_ensure_terminal_session() -> None:
    ensured.append(True)

  async def fake_pump(_attachment, _websocket) -> None:
    await asyncio.sleep(60)

  monkeypatch.setattr(terminal, "ensure_terminal_session", fake_ensure_terminal_session)
  monkeypatch.setattr(terminal, "PtyAttachment", _FakeAttachment)
  monkeypatch.setattr(pty_common, "_pump_pty_to_ws", fake_pump)

  await terminal.run_terminal_attachment(ws)

  attachment = _FakeAttachment.instances[0]
  assert ensured == [True]
  assert attachment.session_id == "terminal"
  assert attachment.spawned is True
  assert attachment.writes == [b"pwd\n"]
  assert attachment.resizes == [(100, 30)]
  assert attachment.closed is True


@pytest.mark.asyncio
async def test_tui_attachment_still_uses_shared_pty_path(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  ensured = []
  _FakeAttachment.instances = []
  ws = _ScriptedWebSocket([
      {"type": PTY_INPUT, "data": _b64(b"help\n")},
      {"type": PTY_RESIZE, "cols": 120, "rows": 40},
  ])

  async def fake_ensure_tmux_session(session_id: str, working_dir: Path) -> None:
    ensured.append((session_id, working_dir))

  async def fake_pump(_attachment, _websocket) -> None:
    await asyncio.sleep(60)

  class FakeSessionManager:
    async def get_session(self, _session_id: str):
      return None

  monkeypatch.setattr(tui, "ensure_tmux_session", fake_ensure_tmux_session)
  monkeypatch.setattr(tui, "PtyAttachment", _FakeAttachment)
  monkeypatch.setattr(pty_common, "_pump_pty_to_ws", fake_pump)
  monkeypatch.setattr("src.api.deps.get_session_manager", lambda: FakeSessionManager())

  await tui.run_tui_attachment(ws, "session-id", tmp_path / "sessions")

  attachment = _FakeAttachment.instances[0]
  assert ensured == [("session-id", tmp_path / "sessions" / "session-id")]
  assert attachment.session_id == "session-id"
  assert attachment.spawned is True
  assert attachment.writes == [b"help\n"]
  assert attachment.resizes == [(120, 40)]
  assert attachment.closed is True


@pytest.mark.asyncio
async def test_terminal_websocket_uses_ws_auth(monkeypatch: pytest.MonkeyPatch) -> None:
  from server import terminal_websocket

  ws = _AcceptingWebSocket()
  checked = []
  attached = []

  async def fake_check_ws_auth(websocket) -> bool:
    checked.append(websocket)
    return True

  async def fake_run_terminal_attachment(websocket) -> None:
    attached.append(websocket)

  monkeypatch.setattr("server._check_ws_auth", fake_check_ws_auth)
  monkeypatch.setattr("src.agents.backends.terminal.run_terminal_attachment", fake_run_terminal_attachment)

  await terminal_websocket(ws)

  assert checked == [ws]
  assert ws.accepted is True
  assert attached == [ws]


@pytest.mark.asyncio
async def test_terminal_websocket_rejects_failed_ws_auth(monkeypatch: pytest.MonkeyPatch) -> None:
  from server import terminal_websocket

  ws = _AcceptingWebSocket()
  attached = []

  async def fake_check_ws_auth(_websocket) -> bool:
    return False

  async def fake_run_terminal_attachment(websocket) -> None:
    attached.append(websocket)

  monkeypatch.setattr("server._check_ws_auth", fake_check_ws_auth)
  monkeypatch.setattr("src.agents.backends.terminal.run_terminal_attachment", fake_run_terminal_attachment)

  await terminal_websocket(ws)

  assert ws.accepted is False
  assert attached == []


def test_pty_client_can_push_clipboard_to_the_browser(monkeypatch: pytest.MonkeyPatch) -> None:
  """A tmux-side copy must reach the browser-facing PTY as OSC 52.

  tmux only emits OSC 52 to a client whose terminfo advertises `Ms`, so the attach client
  has to declare itself as xterm-256color no matter what TERM the server inherited.
  """
  tmux = shutil.which("tmux")
  if tmux is None:
    pytest.skip("tmux binary not available")

  # What the server inherits whenever it was started from inside a tmux pane.
  monkeypatch.setenv("TERM", "screen-256color")
  socket_name = f"charliebot-test-{os.getpid()}-{uuid.uuid4().hex}"
  session_id = uuid.uuid4().hex
  session_name = pty_common.tmux_session_name(session_id)
  monkeypatch.setattr(pty_common, "_TMUX_SOCKET", socket_name)

  def run_tmux(*args: str) -> subprocess.CompletedProcess:
    return subprocess.run([tmux, "-L", socket_name, *args], capture_output=True, text=True, check=False)

  attachment = pty_common.PtyAttachment(session_id)
  try:
    created = run_tmux("new-session", "-d", "-s", session_name, "-x", "80", "-y", "24", "sleep 60")
    assert created.returncode == 0, created.stderr

    attachment.spawn()
    termname = ""
    for _ in range(60):
      listed = run_tmux("list-clients", "-t", session_name, "-F", "#{client_termname}")
      if listed.stdout.strip():
        termname = listed.stdout.strip().splitlines()[-1]
        break
      time.sleep(0.1)
    assert termname == "xterm-256color"

    copied = run_tmux("set-buffer", "-w", "CLIPBOARD-PROBE")
    assert copied.returncode == 0, copied.stderr

    seen = b""
    deadline = time.monotonic() + 5.0
    while time.monotonic() < deadline and b"\x1b]52" not in seen:
      ready, _, _ = select.select([attachment.fd], [], [], 0.2)
      if ready:
        try:
          seen += os.read(attachment.fd, 65536)
        except (BlockingIOError, OSError):
          pass
    assert b"\x1b]52" in seen, "tmux never pushed OSC 52 to the browser-facing PTY"
    assert base64.b64encode(b"CLIPBOARD-PROBE") in seen
  finally:
    attachment.close()
    subprocess.run(
        [tmux, "-L", socket_name, "kill-server"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )
