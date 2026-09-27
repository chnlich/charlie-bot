import pytest
from fastapi import WebSocket

from src.agents.backends import pty_common

# Import-path patch targets for the server's terminal websocket. server.py defines _check_ws_auth
# and its websocket handlers read it as a module global at call time, and the terminal handler
# imports run_terminal_attachment at call time (`from src.agents.backends.terminal import
# run_terminal_attachment` inside terminal_websocket), so monkeypatch.setattr lands both stand-ins
# on their defining module attributes and the handler's reads resolve them.
SERVER_CHECK_WS_AUTH_PATCH_TARGET = "server._check_ws_auth"
TERMINAL_RUN_TERMINAL_ATTACHMENT_PATCH_TARGET = "src.agents.backends.terminal.run_terminal_attachment"


class _AcceptingWebSocket:

  def __init__(self) -> None:
    self.accepted = False

  async def accept(self) -> None:
    self.accepted = True


@pytest.mark.asyncio
async def test_run_tmux_strips_session_env(monkeypatch: pytest.MonkeyPatch) -> None:
  captured: dict[str, dict[str, str]] = {}
  monkeypatch.setenv("CHARLIEBOT_SESSION_ID", "stale-session")
  monkeypatch.setattr(pty_common, "_tmux_binary", lambda: "/usr/bin/tmux")

  class FakeProcess:
    returncode = 0

    async def communicate(self) -> tuple[bytes, bytes]:
      return b"", b""

  async def fake_create_subprocess_exec(*args: object, **kwargs: object) -> FakeProcess:
    captured["env"] = kwargs["env"]
    return FakeProcess()

  monkeypatch.setattr(pty_common.asyncio, "create_subprocess_exec", fake_create_subprocess_exec)

  rc, stderr = await pty_common._run_tmux("has-session", "-t", "charliebot-session")

  assert rc == 0
  assert stderr == ""
  assert "CHARLIEBOT_SESSION_ID" not in captured["env"]


@pytest.mark.asyncio
@pytest.mark.parametrize("auth_ok", [True, False], ids=["accepts", "rejects"])
async def test_terminal_websocket_ws_auth_gate(monkeypatch: pytest.MonkeyPatch, auth_ok: bool) -> None:
  """The attach runs only behind one auth check: a failed check neither accepts the socket nor attaches."""
  from server import terminal_websocket

  ws = _AcceptingWebSocket()
  checked = []
  attached = []

  async def fake_check_ws_auth(websocket: WebSocket) -> bool:
    checked.append(websocket)
    return auth_ok

  async def fake_run_terminal_attachment(websocket: WebSocket) -> None:
    attached.append(websocket)

  monkeypatch.setattr(SERVER_CHECK_WS_AUTH_PATCH_TARGET, fake_check_ws_auth)
  monkeypatch.setattr(TERMINAL_RUN_TERMINAL_ATTACHMENT_PATCH_TARGET, fake_run_terminal_attachment)

  await terminal_websocket(ws)

  assert checked == [ws]
  assert ws.accepted is auth_ok
  assert attached == ([ws] if auth_ok else [])
