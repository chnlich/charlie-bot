import fastapi
import pytest

from src.infra import constants
from src.runtime.agent_process import pty_common

# Import-path patch targets for the terminal websocket. The handler reads check_ws_auth off
# src.runtime.api.auth and run_terminal_attachment off src.features.terminal.terminal at call time,
# so monkeypatch.setattr lands both stand-ins on their defining module attributes and the
# handler's reads resolve them.
CHECK_WS_AUTH_PATCH_TARGET = "src.runtime.api.auth.check_ws_auth"
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
  from src.features.terminal import api as terminal_api

  ws = _AcceptingWebSocket()
  checked = []
  attached = []

  async def fake_check_ws_auth(websocket: fastapi.WebSocket) -> bool:
    checked.append(websocket)
    return auth_ok

  async def fake_run_terminal_attachment(websocket: fastapi.WebSocket) -> None:
    attached.append(websocket)

  monkeypatch.setattr(CHECK_WS_AUTH_PATCH_TARGET, fake_check_ws_auth)
  monkeypatch.setattr(TERMINAL_RUN_TERMINAL_ATTACHMENT_PATCH_TARGET, fake_run_terminal_attachment)

  await terminal_api.terminal_websocket(ws)

  assert checked == [ws]
  assert ws.accepted is auth_ok
  assert attached == ([ws] if auth_ok else [])
