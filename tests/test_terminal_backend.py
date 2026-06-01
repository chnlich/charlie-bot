import asyncio
import base64
import json
from pathlib import Path

import pytest
from fastapi import WebSocketDisconnect

from src.agents.backends import terminal, tui
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
  monkeypatch.setattr(terminal, "_pump_pty_to_ws", fake_pump)

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
  monkeypatch.setattr(tui, "_pump_pty_to_ws", fake_pump)
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
