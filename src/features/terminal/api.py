"""The host-global terminal's websocket route."""

import fastapi

from src.features.terminal import terminal
from src.infra import log_once
from src.runtime.api import auth

log = log_once.LazyStructlogLogger()
router = fastapi.APIRouter()


@router.websocket("/ws/terminal")
async def terminal_websocket(websocket: fastapi.WebSocket) -> None:
  """Attach the browser to the host-global tmux terminal."""
  if not await auth.check_ws_auth(websocket):
    return
  await websocket.accept()
  log.info("terminal_ws_connected")
  try:
    await terminal.run_terminal_attachment(websocket)
  finally:
    log.info("terminal_ws_disconnected")
