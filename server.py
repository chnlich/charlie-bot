"""CharlieBot server entry point."""

import asyncio
import hmac
import json
from contextlib import asynccontextmanager
from pathlib import Path

import structlog
from fastapi import FastAPI, WebSocket, WebSocketDisconnect
from fastapi.staticfiles import StaticFiles
from starlette.middleware.gzip import GZipMiddleware

from src.api import anthropic_proxy, backlog, chat, cron, ext_usage, files, git, internal, latex, pages, sessions, slash, threads, voice
from src.api.auth import AuthMiddleware
from src.api.deps import get_session_manager, get_thread_manager, set_trigger_manager
from src.core import event_types as ET
from src.core.config import get_config
from src.core.http import close_http_client
from src.core.init import init_charliebot_home
from src.core.message_aggregator import MessageAggregator
from src.core.scheduler import Scheduler
from src.core.streaming import streaming_manager
from src.core.triggers import TriggerManager

# Raw event types replaced by aggregator deltas on the wire (kept in sync
# with src.core.sessions._RAW_EVENTS_REPLACED_BY_DELTAS).
_RAW_EVENTS_REPLACED_BY_DELTAS: frozenset[str] = frozenset({ET.ASSISTANT, ET.USER})

log = structlog.get_logger()

# Interval between WebSocket keepalive pings (seconds).
_WS_KEEPALIVE_TIMEOUT = 30.0


async def _check_ws_auth(websocket: WebSocket) -> bool:
  """Validate access-key auth for WebSocket connections.

  Returns True if the connection is authorized, False otherwise
  (and closes the socket with code 4401).
  """
  access_key = get_config().charliebot_access_key
  if not access_key:
    return True
  token = websocket.query_params.get("token", "")
  if hmac.compare_digest(token, access_key):
    return True
  await websocket.close(code=4401)
  return False


@asynccontextmanager
async def _ws_connection(websocket: WebSocket, channels: list[str], log_label: str, **log_context):
  """Handle auth, accept, subscribe, and cleanup for a WebSocket endpoint.

  Yields the websocket if the connection was authorized and accepted, otherwise
  yields None. Subscribes to every channel before yielding and unsubscribes +
  logs disconnection in the finally block.
  """
  if not await _check_ws_auth(websocket):
    yield None
    return
  await websocket.accept()
  log.info(f"{log_label}_connected", **log_context)
  for channel in channels:
    await streaming_manager.subscribe(channel, websocket)
  try:
    yield websocket
  finally:
    for channel in channels:
      await streaming_manager.unsubscribe(channel, websocket)
    log.info(f"{log_label}_disconnected", **log_context)


async def _ws_keepalive(websocket: WebSocket, log_label: str, **log_context) -> None:
  """Hold a WebSocket open with periodic pings until the client disconnects."""
  try:
    while True:
      try:
        await asyncio.wait_for(websocket.receive_text(), timeout=_WS_KEEPALIVE_TIMEOUT)
      except asyncio.TimeoutError:
        await websocket.send_json({"type": "ping"})
  except WebSocketDisconnect:
    pass
  except Exception as e:
    log.info(f"{log_label}_closed", reason=str(e), **log_context)


@asynccontextmanager
async def lifespan(app: FastAPI):
  """Application lifespan: startup and shutdown tasks."""
  cfg = get_config()

  # Ensure home directory structure exists
  await init_charliebot_home()
  log.info("charliebot_home_ready", path=str(cfg.charliebot_home))

  scheduler = Scheduler(cfg)
  app.state.scheduler = scheduler
  await scheduler.start()

  session_mgr = get_session_manager()
  trigger_mgr = TriggerManager(cfg, session_mgr)
  set_trigger_manager(trigger_mgr)
  app.state.trigger_mgr = trigger_mgr
  await trigger_mgr.recover_pending()

  await ext_usage.start_poller()

  yield

  await ext_usage.stop_poller()
  await close_http_client()
  await scheduler.stop()
  await streaming_manager.close_all()
  log.info("charliebot_shutdown")


app = FastAPI(
  title="CharlieBot",
  description="Multi-agent Claude Code orchestration system",
  version="0.1.0",
  lifespan=lifespan,
)

app.add_middleware(GZipMiddleware, minimum_size=1000)
app.add_middleware(AuthMiddleware)

# Page router (GET / — Jinja2 rendered)
app.include_router(pages.router, tags=["pages"])

# API routers
app.include_router(sessions.router, prefix="/api/sessions", tags=["sessions"])
app.include_router(chat.router, prefix="/api/chat", tags=["chat"])
app.include_router(threads.router, prefix="/api/threads", tags=["threads"])
app.include_router(voice.router, prefix="/api/voice", tags=["voice"])
app.include_router(latex.router, prefix="/api/latex", tags=["latex"])
app.include_router(backlog.router, prefix="/api/backlog", tags=["backlog"])
app.include_router(internal.router, prefix="/api/internal", tags=["internal"])
app.include_router(slash.router, prefix="/api/slash", tags=["slash"])
app.include_router(cron.router, prefix="/api/cron", tags=["cron"])
app.include_router(git.router, prefix="/api/git", tags=["git"])
app.include_router(ext_usage.router, prefix="/api", tags=["ext-usage"])
app.include_router(anthropic_proxy.router, prefix="/api/anthropic-proxy", tags=["anthropic-proxy"])

# File server (filesystem browser)
app.include_router(files.router, prefix="/files", tags=["files"])


# ---------------------------------------------------------------------------
# WebSocket endpoint for session-level events (master CC + worker summaries)
# ---------------------------------------------------------------------------


@app.websocket("/ws/sessions/{session_id}")
async def session_websocket(websocket: WebSocket, session_id: str):
  """Push session-level events (master CC output, worker summaries) to the browser."""
  # Auth + accept happen inline here because cursor negotiation (unique to this
  # endpoint) must read a message from the accepted socket before subscribing.
  if not await _check_ws_auth(websocket):
    return
  await websocket.accept()
  log.info("session_ws_connected", session_id=session_id)

  cursor = 0
  try:
    raw = await asyncio.wait_for(websocket.receive_text(), timeout=5.0)
    msg = json.loads(raw)
    if msg.get("type") == "cursor":
      cursor = int(msg.get("index", 0))
  except WebSocketDisconnect:
    log.debug("session_ws_early_disconnect", session_id=session_id)
    return
  except (asyncio.TimeoutError, json.JSONDecodeError, ValueError, TypeError) as e:
    log.debug("session_ws_cursor_parse_failed", session_id=session_id, error=str(e))

  # Subscribe BEFORE catchup so no events are lost between catchup and subscribe.
  channel = f"session:{session_id}"
  await streaming_manager.subscribe(channel, websocket)
  await streaming_manager.subscribe("sidebar", websocket)
  try:
    session_mgr = get_session_manager()
    meta = await session_mgr.get_session(session_id)
    try:
      sent, total_event_count = await _send_session_catchup(websocket, session_mgr, session_id, cursor, meta)
      await websocket.send_json({"type": "catchup_complete"})
      log.debug(
          "session_ws_catchup_sent",
          session_id=session_id,
          cursor=cursor,
          total=total_event_count,
          sent=sent,
      )
    except Exception as e:
      log.warning("session_ws_catchup_failed", session_id=session_id, error=str(e))

    cfg = get_config()
    backend_option = cfg.get_backend_option(meta.backend) if meta and meta.backend else None
    if backend_option is not None and backend_option.type == "tui-cli":
      from src.agents.backends.tui import run_tui_attachment
      await run_tui_attachment(websocket, session_id, cfg.sessions_dir)
    else:
      await _ws_keepalive(websocket, "session_ws", session_id=session_id)
  finally:
    await streaming_manager.unsubscribe(channel, websocket)
    await streaming_manager.unsubscribe("sidebar", websocket)
    log.info("session_ws_disconnected", session_id=session_id)


async def _send_session_catchup(
    websocket: WebSocket,
    session_mgr,
    session_id: str,
    cursor: int,
    meta,
) -> tuple[int, int]:
  """Send catchup frames, fast-skipping replay when the client cursor is current."""
  event_index_offset = meta.archive_offset if meta else 0
  total_event_count: int | None
  try:
    total_event_count = await asyncio.to_thread(session_mgr.get_chat_event_count_sync, session_id, meta)
  except Exception as e:
    log.warning("session_ws_event_count_failed", session_id=session_id, error=str(e))
    total_event_count = None

  if total_event_count is not None and cursor >= total_event_count:
    return 0, total_event_count

  events = await asyncio.to_thread(session_mgr.load_chat_events_sync, session_id)
  sent = await _replay_aggregated_catchup(
      websocket,
      events,
      cursor,
      session_id,
      event_index_offset=event_index_offset,
  )
  return sent, event_index_offset + len(events)


async def _replay_aggregated_catchup(
  websocket: WebSocket,
  events: list[dict],
  cursor: int,
  session_id: str,
  *,
  event_index_offset: int = 0,
) -> int:
  """Replay events past *cursor* as aggregator deltas + raw side-effect events.

  Walks the full event list to keep the aggregator state aligned with how the
  client's SSR/SPA aggregator processed events[0..cursor-1]; deltas from events
  before the cursor are dropped because the client has already rendered them.
  Raw `assistant`/`user` events are suppressed because their content is
  represented by `message`/`stream` deltas on the wire. Returns the number of
  frames sent.
  """
  aggregator = MessageAggregator(event_index_offset=event_index_offset)
  sent = 0
  latest_stream: dict | None = None
  for idx, ev in enumerate(events):
    global_idx = event_index_offset + idx
    deltas = list(aggregator.feed(ev))
    if global_idx < cursor:
      continue
    for delta in deltas:
      if delta["type"] == "stream":
        # Coalesce: only the most recent stream snapshot matters for catchup.
        latest_stream = delta
        continue
      # A `message` commit makes any buffered stream obsolete -- the commit
      # carries the same (or more complete) content and the client clears the
      # streaming preview when it appends a committed bubble.
      latest_stream = None
      try:
        await websocket.send_json(delta)
        sent += 1
      except Exception as e:
        log.debug("session_ws_catchup_send_failed", session_id=session_id, error=str(e))
        return sent
    if ev.get("type") in _RAW_EVENTS_REPLACED_BY_DELTAS:
      continue
    payload = dict(ev)
    payload.setdefault("event_index", global_idx)
    try:
      await websocket.send_json(payload)
      sent += 1
    except Exception as e:
      log.debug("session_ws_catchup_send_failed", session_id=session_id, error=str(e))
      return sent
  if latest_stream is not None:
    try:
      await websocket.send_json(latest_stream)
      sent += 1
    except Exception as e:
      log.debug("session_ws_catchup_send_failed", session_id=session_id, error=str(e))
  return sent


# ---------------------------------------------------------------------------
# WebSocket endpoint for live Worker output
# ---------------------------------------------------------------------------


@app.websocket("/ws/threads/{thread_id}")
async def thread_websocket(websocket: WebSocket, thread_id: str):
  """
  Stream live Worker events to the browser.
  On connect, sends all historical events first (catch-up), then live events.
  """
  async with _ws_connection(websocket, [thread_id], "ws", thread_id=thread_id) as ws:
    if ws is None:
      return
    # Send catch-up events from on-disk log
    # Find the events.jsonl for this thread (search across all sessions)
    thread_mgr = get_thread_manager()
    events_file = await asyncio.to_thread(thread_mgr.resolve_events_file, thread_id)
    if events_file and events_file.exists():
      try:
        lines = await asyncio.to_thread(events_file.read_text, "utf-8")
        for line in lines.splitlines():
          line = line.strip()
          if line:
            try:
              await ws.send_text(line)
            except Exception as e:
              log.debug("ws_catchup_send_failed", thread_id=thread_id, error=str(e))
              return
      except Exception as e:
        log.warning("ws_catchup_failed", thread_id=thread_id, error=str(e))

    # Signal end of catch-up
    try:
      await ws.send_json({"type": "catchup_complete"})
    except Exception as e:
      log.debug("ws_catchup_complete_failed", thread_id=thread_id, error=str(e))
      return

    await _ws_keepalive(ws, "ws", thread_id=thread_id)


# ---------------------------------------------------------------------------
# WebSocket endpoint for the host-global terminal
# ---------------------------------------------------------------------------


@app.websocket("/ws/terminal")
async def terminal_websocket(websocket: WebSocket):
  """Attach the browser to the host-global tmux terminal."""
  if not await _check_ws_auth(websocket):
    return
  await websocket.accept()
  log.info("terminal_ws_connected")
  try:
    from src.agents.backends.terminal import run_terminal_attachment

    await run_terminal_attachment(websocket)
  finally:
    log.info("terminal_ws_disconnected")


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
  ssl_kwargs = {}
  if cfg.ssl_certfile and cfg.ssl_keyfile:
    ssl_kwargs['ssl_certfile'] = cfg.ssl_certfile
    ssl_kwargs['ssl_keyfile'] = cfg.ssl_keyfile
  uvicorn.run(
    "server:app",
    host=cfg.server_host,
    port=cfg.server_port,
    reload=False,
    log_level="info",
    timeout_graceful_shutdown=5,
    **ssl_kwargs,
  )
