"""CharlieBot server entry point."""

import asyncio
import json
from contextlib import asynccontextmanager
from pathlib import Path

import structlog
from fastapi import FastAPI, WebSocket, WebSocketDisconnect
from fastapi.staticfiles import StaticFiles

from src.api import chat, internal, pages, sessions, threads, voice
from src.core.config import get_config
from src.core.init import init_charliebot_home
from src.core.sessions import SessionManager
from src.core.streaming import streaming_manager

log = structlog.get_logger()


@asynccontextmanager
async def lifespan(app: FastAPI):
  """Application lifespan: startup and shutdown tasks."""
  cfg = get_config()

  # Ensure home directory structure exists
  await init_charliebot_home()
  log.info("charliebot_home_ready", path=str(cfg.charliebot_home))

  yield

  log.info("charliebot_shutdown")


app = FastAPI(
  title="CharlieBot",
  description="Multi-agent Claude Code orchestration system",
  version="0.1.0",
  lifespan=lifespan,
)

# Page router (GET / — Jinja2 rendered)
app.include_router(pages.router, tags=["pages"])

# API routers
app.include_router(sessions.router, prefix="/api/sessions", tags=["sessions"])
app.include_router(chat.router, prefix="/api/chat", tags=["chat"])
app.include_router(threads.router, prefix="/api/threads", tags=["threads"])
app.include_router(voice.router, prefix="/api/voice", tags=["voice"])
app.include_router(internal.router, prefix="/api/internal", tags=["internal"])


# ---------------------------------------------------------------------------
# WebSocket endpoint for session-level events (master CC + worker summaries)
# ---------------------------------------------------------------------------


@app.websocket("/ws/sessions/{session_id}")
async def session_websocket(websocket: WebSocket, session_id: str):
  """Push session-level events (master CC output, worker summaries) to the browser."""
  await websocket.accept()
  channel = f"session:{session_id}"
  log.info("session_ws_connected", session_id=session_id)

  # Send catch-up events from chat_events.jsonl
  cfg = get_config()
  session_mgr = SessionManager(cfg)
  try:
    events = session_mgr.load_chat_events_sync(session_id)
    for event in events:
      try:
        await websocket.send_json(event)
      except Exception as e:
        log.debug("session_ws_catchup_send_failed", session_id=session_id, error=str(e))
        return
    await websocket.send_json({"type": "catchup_complete"})
  except Exception as e:
    log.warning("session_ws_catchup_failed", session_id=session_id, error=str(e))

  await streaming_manager.subscribe(channel, websocket)
  try:
    while True:
      try:
        await asyncio.wait_for(websocket.receive_text(), timeout=30.0)
      except asyncio.TimeoutError:
        await websocket.send_json({"type": "ping"})
  except WebSocketDisconnect:
    pass
  except Exception as e:
    log.info("session_ws_closed", session_id=session_id, reason=str(e))
  finally:
    await streaming_manager.unsubscribe(channel, websocket)
    log.info("session_ws_disconnected", session_id=session_id)


# ---------------------------------------------------------------------------
# WebSocket endpoint for live Worker output
# ---------------------------------------------------------------------------


@app.websocket("/ws/threads/{thread_id}")
async def thread_websocket(websocket: WebSocket, thread_id: str):
  """
  Stream live Worker events to the browser.
  On connect, sends all historical events first (catch-up), then live events.
  """
  await websocket.accept()
  log.info("ws_connected", thread_id=thread_id)

  # Send catch-up events from on-disk log
  cfg = get_config()
  # Find the events.jsonl for this thread (search across all sessions)
  events_file = _find_events_file(thread_id, cfg)
  if events_file and events_file.exists():
    try:
      with open(events_file, "r", encoding="utf-8") as f:
        for line in f:
          line = line.strip()
          if line:
            try:
              await websocket.send_text(line)
            except Exception as e:
              log.debug("ws_catchup_send_failed", thread_id=thread_id, error=str(e))
              return
    except Exception as e:
      log.warning("ws_catchup_failed", thread_id=thread_id, error=str(e))

  # Signal end of catch-up
  try:
    await websocket.send_json({"type": "catchup_complete"})
  except Exception as e:
    log.debug("ws_catchup_complete_failed", thread_id=thread_id, error=str(e))
    return

  # Subscribe for live events
  await streaming_manager.subscribe(thread_id, websocket)
  try:
    while True:
      # Keep connection alive; client may send pings
      try:
        await asyncio.wait_for(websocket.receive_text(), timeout=30.0)
      except asyncio.TimeoutError:
        # Send keepalive ping
        await websocket.send_json({"type": "ping"})
  except WebSocketDisconnect:
    pass
  except Exception as e:
    log.info("ws_closed", thread_id=thread_id, reason=str(e))
  finally:
    await streaming_manager.unsubscribe(thread_id, websocket)
    log.info("ws_disconnected", thread_id=thread_id)


def _find_events_file(thread_id: str, cfg) -> Path | None:
  """Search for events.jsonl across all session thread directories."""
  if not cfg.sessions_dir.exists():
    return None
  for session_dir in cfg.sessions_dir.iterdir():
    if not session_dir.is_dir():
      continue
    candidate = session_dir / "threads" / thread_id / "data" / "events.jsonl"
    if candidate.exists():
      return candidate
  return None


# ---------------------------------------------------------------------------
# Static files (CSS, JS, images — NOT the SPA)
# ---------------------------------------------------------------------------

_static_dir = Path(__file__).parent / "web" / "static"
if _static_dir.exists():
  app.mount("/static", StaticFiles(directory=str(_static_dir)), name="static")


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------

if __name__ == "__main__":
  import uvicorn

  cfg = get_config()
  uvicorn.run(
    "server:app",
    host="0.0.0.0",
    port=cfg.server_port,
    reload=False,
    log_level="info",
  )
