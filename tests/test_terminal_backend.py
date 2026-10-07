import fastapi
import pytest

from src.infra import constants
from src.runtime.agent_process import pty_common

# Import-path patch targets for the server's terminal websocket. server.py defines _check_ws_auth
# and its websocket handlers read it as a module global at call time, and the terminal handler
# imports run_terminal_attachment at call time (`from src.features.terminal.terminal import
# run_terminal_attachment` inside terminal_websocket), so monkeypatch.setattr lands both stand-ins
# on their defining module attributes and the handler's reads resolve them.
SERVER_CHECK_WS_AUTH_PATCH_TARGET = "server._check_ws_auth"
TERMINAL_RUN_TERMINAL_ATTACHMENT_PATCH_TARGET = "src.features.terminal.terminal.run_terminal_attachment"


class _AcceptingWebSocket:

  def __init__(self) -> None:
    self.accepted = False

  async def accept(self) -> None:
    self.accepted = True


@pytest.mark.asyncio
async def test_run_tmux_strips_session_env(monkeypatch: pytest.MonkeyPatch) -> None:
  captured: dict[str, dict[str, str]] = {}
  monkeypatch.setenv(constants.SESSION_ID_ENV_VAR, "stale-session")
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
  assert constants.SESSION_ID_ENV_VAR not in captured["env"]


@pytest.mark.asyncio
@pytest.mark.parametrize("auth_ok", [True, False], ids=["accepts", "rejects"])
async def test_terminal_websocket_ws_auth_gate(monkeypatch: pytest.MonkeyPatch, auth_ok: bool) -> None:
  """The attach runs only behind one auth check: a failed check neither accepts the socket nor attaches."""
  import server

  ws = _AcceptingWebSocket()
  checked = []
  attached = []

  async def fake_check_ws_auth(websocket: fastapi.WebSocket) -> bool:
    checked.append(websocket)
    return auth_ok

  async def fake_run_terminal_attachment(websocket: fastapi.WebSocket) -> None:
    attached.append(websocket)

  monkeypatch.setattr(SERVER_CHECK_WS_AUTH_PATCH_TARGET, fake_check_ws_auth)
  monkeypatch.setattr(TERMINAL_RUN_TERMINAL_ATTACHMENT_PATCH_TARGET, fake_run_terminal_attachment)

  await server.terminal_websocket(ws)

  assert checked == [ws]
  assert ws.accepted is auth_ok
  assert attached == ([ws] if auth_ok else [])
