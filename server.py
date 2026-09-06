"""CharlieBot server entry point."""

import asyncio
import hmac
import json
import time
from contextlib import asynccontextmanager, suppress
from datetime import datetime
from pathlib import Path

import structlog
from fastapi import FastAPI, WebSocket, WebSocketDisconnect
from fastapi.staticfiles import StaticFiles
from starlette.datastructures import Headers, MutableHeaders
from starlette.middleware.gzip import GZipMiddleware, GZipResponder
from starlette.types import ASGIApp, Message, Receive, Scope, Send

from src.agents import transcriber
from src.api import (
    anthropic_proxy,
    backlog,
    chat,
    code_server,
    cron,
    diag,
    ext_usage,
    files,
    git,
    internal,
    latex,
    pages,
    sessions,
    slash,
    threads,
    voice,
)
from src.api.auth import AuthMiddleware
from src.api.deps import get_session_manager, get_thread_manager, set_trigger_manager
from src.core import timeouts
from src.core.buildinfo import init_build_info
from src.core.config import CharlieBotConfig, get_config
from src.core.http import close_http_client
from src.core.init import (
    init_charliebot_home,
    reconcile_master_identity,
    run_crash_recovery,
)
from src.core.message_aggregator import MessageAggregator
from src.core.models import BackendType, SessionMetadata, utc_now
from src.core.scheduler import Scheduler
from src.core.sessions import _RAW_EVENTS_REPLACED_BY_DELTAS, SessionManager
from src.core.streaming import streaming_manager
from src.core.tasks import create_logged_task
from src.core.triggers import TriggerManager

log = structlog.get_logger()

# Interval between WebSocket keepalive pings (seconds).
_WS_KEEPALIVE_TIMEOUT = 30.0

# Catchup replay slicing: events walked and frames rendered per on-loop slice.
# The walk measures ~1.2 µs per event; a ticker waking mid-slice waits out the
# rest of the slice (gap = its 5 ms sleep + the slice remainder), so slices stay
# near 1 ms to keep the service delay under the GIL-handoff alternative.
_CATCHUP_SLICE_EVENTS = 400
_CATCHUP_RENDER_SLICE = 4

# A whole-body deflate below this size costs less inline than one executor
# round-trip (~104 µs measured on this host); above it the thread hop wins.
_OFFLOOP_GZIP_MIN_BODY = 8192


class _OffLoopWholeBodyGZipResponder(GZipResponder):
  """Whole-body responses deflate in a worker thread; streaming chunks stay inline.

  Starlette's GZipResponder runs the whole-body deflate inside the send path, so a
  large JSON page freezes the event loop for the whole compression (17 ms measured
  on the 559 KB events page). Every JSON route ships its body as one message, so
  one to_thread hop takes that deflate off the loop; a streaming body keeps the
  inline per-chunk path, whose chunks are small and whose gzip file state must
  not cross threads between writes.
  """

  async def send_with_compression(self, message: Message) -> None:
    if (message["type"] == "http.response.body" and not self.started and not self.content_encoding_set and
        not self.content_type_is_excluded and not message.get("more_body", False) and
        len(message.get("body", b"")) >= _OFFLOOP_GZIP_MIN_BODY):
      body = await asyncio.to_thread(self.apply_compression, message["body"], more_body=False)
      headers = MutableHeaders(raw=self.initial_message["headers"])
      headers.add_vary_header("Accept-Encoding")
      if body != message["body"]:
        headers["Content-Encoding"] = self.content_encoding
        headers["Content-Length"] = str(len(body))
        message["body"] = body
      await self.send(self.initial_message)
      await self.send(message)
      return
    await super().send_with_compression(message)


class _CharlieBotGZipMiddleware(GZipMiddleware):
  """Skip HTTP transport compression for already-compressed trace files."""

  async def __call__(self, scope: Scope, receive: Receive, send: Send) -> None:
    if scope["type"] == "http" and scope["path"] == "/perfetto/merged":
      await self.app(scope, receive, send)
      return
    if scope["type"] == "http" and "gzip" in Headers(scope=scope).get("Accept-Encoding", ""):
      responder = _OffLoopWholeBodyGZipResponder(self.app, self.minimum_size, compresslevel=self.compresslevel)
      await responder(scope, receive, send)
      return
    await super().__call__(scope, receive, send)


class _RequestLogMiddleware:
  """Emit exactly one structlog http_request event per HTTP response.

  Pure ASGI like the GZip middleware above: send is wrapped only to capture the
  status off http.response.start (nothing is buffered, so streaming and
  StaticFiles bodies still log exactly once), and the event fires when the
  inner app returns. The query string is deliberately excluded from `path` —
  the terminal WS carries its access credential in ?token= — so credentials
  never reach the log. Mounted last, hence outermost, so AuthMiddleware's 401s
  are logged too. An inner-app exception logs status=500 with the exception's
  class name and re-raises unchanged (uvicorn's WARNING+ channel renders the
  traceback); with log_level="warning" in __main__ this middleware is the
  single producer of request logs.
  """

  def __init__(self, app: ASGIApp) -> None:
    self.app = app

  async def __call__(self, scope: Scope, receive: Receive, send: Send) -> None:
    if scope["type"] != "http":
      await self.app(scope, receive, send)
      return
    started = time.monotonic()
    status: int | None = None

    async def send_capturing_status(message: Message) -> None:
      nonlocal status
      if message["type"] == "http.response.start":
        status = message["status"]
      await send(message)

    try:
      await self.app(scope, receive, send_capturing_status)
    except Exception as e:
      self._log_http_request(scope, 500, started, error=type(e).__name__)
      raise
    self._log_http_request(scope, status, started)

  def _log_http_request(self, scope: Scope, status: int | None, started: float, *, error: str | None = None) -> None:
    """The one http_request log site: five fields, plus `error` on the exception path only."""
    client = scope.get("client")
    fields: dict[str, object] = {
        "method": scope["method"],
        "path": scope["path"],
        "status": status,
        "duration_ms": round((time.monotonic() - started) * 1000),
        "client": client[0] if client else "-",
    }
    if error is not None:
      fields["error"] = error
    log.info("http_request", **fields)


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


async def _ws_keepalive(websocket: WebSocket, log_label: str, **log_context) -> None:
  """Hold a WebSocket open with periodic pings until the client disconnects."""
  try:
    while True:
      try:
        await asyncio.wait_for(websocket.receive_text(), timeout=_WS_KEEPALIVE_TIMEOUT)
      except TimeoutError:
        await websocket.send_json({"type": "ping"})
  except WebSocketDisconnect:
    pass
  except Exception as e:
    log.info(f"{log_label}_closed", reason=str(e), **log_context)


async def _run_crash_recovery(cfg: CharlieBotConfig, boot_time: datetime, identity: asyncio.Task | None = None) -> None:
  """Background startup recovery; logs completion and never swallows failures.

  Wraps init.run_crash_recovery so an exception surfaces loudly instead of
  vanishing into the event loop, and reports the recovered count + elapsed time
  once the deferred scan finishes. *identity* is the lifespan's one shielded
  reconcile_master_identity task, forwarded for the replay pass.
  """
  started = utc_now()
  try:
    recovered = await run_crash_recovery(
        cfg, boot_time, get_session_manager(), get_thread_manager(), master_identity=identity)
    elapsed_ms = round((utc_now() - started).total_seconds() * 1000)
    log.info("crash_recovery_done", count=recovered, elapsed_ms=elapsed_ms)
  except Exception:
    log.exception("crash_recovery_failed")


async def _run_slack_backfill(cfg: CharlieBotConfig, session_mgr: SessionManager, recovery_task: asyncio.Task) -> None:
  """Report Slack summons lost across the restart, once recovery has had its chance.

  Waits on the crash-recovery task first so re-attach and the user-message
  replay have already answered everything they can; whatever is still
  unanswered after that is genuinely lost and gets a notice in its thread.
  """
  from src.core.slack_listener import (
      backfill_lost_summons,  # lazy: avoids import cycle at module scope
  )

  await recovery_task
  reported = await backfill_lost_summons(cfg, session_mgr)
  log.info("slack_backfill_done", count=reported)


@asynccontextmanager
async def lifespan(app: FastAPI):
  """Application lifespan: startup and shutdown tasks."""
  cfg = get_config()
  boot_time = utc_now()

  # Capture build info (git SHA + start time) for the /api/internal/version endpoint
  # and the CLI version-skew hint. Runs synchronously; ~2 s worst case for git rev-parse.
  init_build_info()

  # Ensure home directory structure exists (fast, mandatory part of startup).
  await init_charliebot_home()
  log.info("charliebot_home_ready", path=str(cfg.charliebot_home))

  # Crash recovery / worktree quarantine / stale-thinking cleanup scans every
  # thread's metadata (O(history)). Run it off the critical path so the server
  # reaches readiness immediately; boot_time guards against killing a worker
  # spawned during the recovery window.
  #
  # The master_run identity judgment is the exception: master_run is a single
  # slot per session that a new turn overwrites unconditionally, so the
  # judgment must complete before any door that can start a new turn — the
  # worker-finalize chain dispatched by this same background task,
  # scheduler.start(), and trigger_mgr.recover_pending() below. Barrier on it
  # with a timeout; the shield keeps the one judgment running past the bound
  # and the recovery task re-awaits that same task for the replay pass.
  session_mgr = get_session_manager()
  identity = asyncio.create_task(reconcile_master_identity(cfg, session_mgr, boot_time))
  try:
    await asyncio.wait_for(asyncio.shield(identity), timeout=timeouts.MASTER_IDENTITY_BARRIER_TIMEOUT)
  except TimeoutError:
    log.warning("master_identity_barrier_timeout", timeout_s=timeouts.MASTER_IDENTITY_BARRIER_TIMEOUT)
  except Exception:
    pass  # reported where the task is awaited: crash recovery logs it loudly
  app.state.recovery_task = asyncio.create_task(_run_crash_recovery(cfg, boot_time, identity))
  app.state.speech_model_task = transcriber.start_model_provisioning(cfg)

  scheduler = Scheduler(cfg, session_mgr)
  app.state.scheduler = scheduler
  await scheduler.start()

  trigger_mgr = TriggerManager(cfg, session_mgr)
  set_trigger_manager(trigger_mgr)
  app.state.trigger_mgr = trigger_mgr
  await trigger_mgr.recover_pending()

  await ext_usage.start_poller()

  slack_listener_task = None
  if cfg.slack_bot_token and cfg.slack_app_token and cfg.slack_allowed_user_ids:
    from src.core.slack_listener import (
        run_listener,  # lazy: avoids import cycle at module scope
    )

    slack_listener_task = create_logged_task(run_listener(cfg, session_mgr), name="slack-listener")
    app.state.slack_listener_task = slack_listener_task
    app.state.slack_backfill_task = create_logged_task(
        _run_slack_backfill(cfg, session_mgr, app.state.recovery_task), name="slack-backfill")
    log.info("slack_entrypoint_started")
  else:
    log.info("slack_entrypoint_off")

  log.info("server_ready", ready_in_ms=round((utc_now() - boot_time).total_seconds() * 1000))
  yield

  speech_model_task = getattr(app.state, "speech_model_task", None)
  if speech_model_task is not None and not speech_model_task.done():
    speech_model_task.cancel()
    with suppress(asyncio.CancelledError):
      await speech_model_task
  for attr in ("slack_listener_task", "slack_backfill_task"):
    task = getattr(app.state, attr, None)
    if task is not None and not task.done():
      task.cancel()
      with suppress(asyncio.CancelledError):
        await task
  await ext_usage.stop_poller()
  await close_http_client()
  await scheduler.stop()
  await streaming_manager.close_all()
  pages.shutdown_merge_executor()
  log.info("charliebot_shutdown")


app = FastAPI(
    title="CharlieBot",
    description="Multi-agent Claude Code orchestration system",
    version="0.1.0",
    lifespan=lifespan,
)

# The deflate sits on the client-visible send path (send awaits the off-loop
# compression), so every big page pays it per fetch: the 633 KB events page
# measures 15.0 ms at level 6 vs 5.5 ms at level 1 (wire 165.7 KB vs 201.1 KB).
app.add_middleware(_CharlieBotGZipMiddleware, minimum_size=1000, compresslevel=1)
app.add_middleware(AuthMiddleware)
# Added last, so starlette's insert(0)/reversed build order makes it the
# outermost user middleware: AuthMiddleware's 401 responses are logged too.
app.add_middleware(_RequestLogMiddleware)

# Page router (GET / — Jinja2 rendered)
app.include_router(pages.router, tags=["pages"])

# API routers
app.include_router(sessions.router, prefix="/api/sessions", tags=["sessions"])
app.include_router(chat.router, prefix="/api/chat", tags=["chat"])
app.include_router(threads.router, prefix="/api/threads", tags=["threads"])
app.include_router(latex.router, prefix="/api/latex", tags=["latex"])
app.include_router(backlog.router, prefix="/api/backlog", tags=["backlog"])
app.include_router(internal.router, prefix="/api/internal", tags=["internal"])
app.include_router(slash.router, prefix="/api/slash", tags=["slash"])
app.include_router(cron.router, prefix="/api/cron", tags=["cron"])
app.include_router(diag.router, prefix="/api/diag", tags=["diag"])
app.include_router(git.router, prefix="/api/git", tags=["git"])
app.include_router(code_server.router, prefix="/api/code-server", tags=["code-server"])
app.include_router(ext_usage.router, prefix="/api", tags=["ext-usage"])
app.include_router(anthropic_proxy.router, prefix="/api/anthropic-proxy", tags=["anthropic-proxy"])

# File server (filesystem browser). The same router is mounted under two prefixes, so both
# reach one handler and one path resolution. "/absolute_filepath" is the prefix written into
# chat text: it names what has to follow it, so a link missing its absolute prefix reads as
# wrong where it is written. "/files" stays as the form the UI builds and older links carry.
app.include_router(files.router, prefix="/files", tags=["files"])
app.include_router(files.router, prefix="/absolute_filepath", tags=["files"])

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
    raw = await asyncio.wait_for(websocket.receive_text(), timeout=timeouts.SESSION_WS_CURSOR_TIMEOUT)
    msg = json.loads(raw)
    if msg.get("type") == "cursor":
      cursor = int(msg.get("index", 0))
  except WebSocketDisconnect:
    log.debug("session_ws_early_disconnect", session_id=session_id)
    return
  except (TimeoutError, json.JSONDecodeError, ValueError, TypeError) as e:
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
    if backend_option is not None and backend_option.type == BackendType.TUI_CLI:
      from src.agents.backends.tui import run_tui_attachment
      await run_tui_attachment(websocket, session_id, cfg.sessions_dir)
    else:
      await _ws_keepalive(websocket, "session_ws", session_id=session_id)
  finally:
    await streaming_manager.unsubscribe(channel, websocket)
    await streaming_manager.unsubscribe("sidebar", websocket)
    log.info("session_ws_disconnected", session_id=session_id)


@app.websocket("/ws/voice/{session_id}")
async def voice_websocket(websocket: WebSocket, session_id: str):
  """Receive PCM audio and stream local transcription updates to the browser."""
  if not await _check_ws_auth(websocket):
    return
  await websocket.accept()
  await voice.handle_voice_websocket(websocket, session_id)


async def _send_session_catchup(
    websocket: WebSocket,
    session_mgr: SessionManager,
    session_id: str,
    cursor: int,
    meta: SessionMetadata | None,
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


class _CatchupWalk:
  """Incremental catchup walk: the aggregator state and frame buffer behind `_catchup_frames`.

  The walk is pure-Python CPU at ~1.2 µs per event; fed in slices from the event
  loop with a yield between slices, no slice holds the loop longer than the
  poll cadences' 5 ms resolution, while a whole-corpus thread run parks the
  loop behind GIL handoffs for its full span (measured 10 ms loop-lag per
  replay). `feed_slice` processes one event range; `finish` emits the ordered
  frame list.
  """

  def __init__(self, cursor: int, event_index_offset: int = 0) -> None:
    self._cursor = cursor
    self._event_index_offset = event_index_offset
    self._aggregator = MessageAggregator(event_index_offset=event_index_offset)
    self._frames: list[dict] = []
    self._latest_stream: dict | None = None

  def feed_slice(self, events: list[dict], start: int, end: int) -> None:
    """Feed events[start:end]; *start* is the slice's index in the full event list."""
    for idx in range(start, end):
      ev = events[idx]
      global_idx = self._event_index_offset + idx
      deltas = list(self._aggregator.feed(ev))
      if global_idx < self._cursor:
        continue
      for delta in deltas:
        if delta["type"] == "stream":
          # Coalesce: only the most recent stream snapshot matters for catchup.
          self._latest_stream = delta
          continue
        # A `message` commit makes any buffered stream obsolete -- the commit
        # carries the same (or more complete) content and the client clears the
        # streaming preview when it appends a committed bubble.
        self._latest_stream = None
        self._frames.append(delta)
      if ev.get("type") in _RAW_EVENTS_REPLACED_BY_DELTAS:
        continue
      payload = dict(ev)
      payload.setdefault("event_index", global_idx)
      self._frames.append(payload)

  def finish(self) -> list[dict]:
    if self._latest_stream is not None:
      self._frames.append(self._latest_stream)
    return self._frames


def _catchup_frames(events: list[dict], cursor: int, *, event_index_offset: int = 0) -> list[dict]:
  """Build the ordered catchup frame list for events past *cursor*.

  Walks the full event list to keep the aggregator state aligned with how the
  client's SSR/SPA aggregator processed events[0..cursor-1]; deltas from events
  before the cursor are dropped from the result because the client has already
  rendered them. Raw events in `_RAW_EVENTS_REPLACED_BY_DELTAS` are suppressed
  because their content is represented by `message`/`stream` deltas on the wire.
  Sync whole-corpus form; the live replay runs the same walk sliced through
  `_CatchupWalk` (see its docstring for why).
  """
  walk = _CatchupWalk(cursor, event_index_offset=event_index_offset)
  walk.feed_slice(events, 0, len(events))
  return walk.finish()


def _render_frames(frames: list[dict]) -> list[str]:
  """Render each catchup frame to the exact bytes WebSocket.send_json would send.

  Starlette's WebSocket.send_json is ``json.dumps(data, separators=(",", ":"),
  ensure_ascii=False)`` plus a text-mode send; the parameters here must match it,
  or the wire bytes change.
  """
  return [json.dumps(frame, separators=(",", ":"), ensure_ascii=False) for frame in frames]


async def _replay_aggregated_catchup(
    websocket: WebSocket,
    events: list[dict],
    cursor: int,
    session_id: str,
    *,
    event_index_offset: int = 0,
) -> int:
  """Replay events past *cursor* as aggregator deltas + raw side-effect events.

  The frame list is built by `_CatchupWalk` in on-loop slices (a yield between
  slices keeps every loop hold under the poll cadences' resolution) and rendered
  to wire text by `_render_frames` under the same slicing, then sent in order;
  the stop-at-first-send failure count is unchanged, so the wire bytes and
  failure shape match the per-frame send_json form. Returns the number of
  frames sent.
  """
  walk = _CatchupWalk(cursor, event_index_offset=event_index_offset)
  for start in range(0, len(events), _CATCHUP_SLICE_EVENTS):
    walk.feed_slice(events, start, min(start + _CATCHUP_SLICE_EVENTS, len(events)))
    await asyncio.sleep(0)
  frames = walk.finish()
  texts: list[str] = []
  for start in range(0, len(frames), _CATCHUP_RENDER_SLICE):
    texts.extend(_render_frames(frames[start:start + _CATCHUP_RENDER_SLICE]))
    await asyncio.sleep(0)
  sent = 0
  for text in texts:
    try:
      await websocket.send_text(text)
      sent += 1
    except Exception as e:
      log.debug("session_ws_catchup_send_failed", session_id=session_id, error=str(e))
      return sent
  return sent


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
  uvicorn.run(
      "server:app",
      host=cfg.server_host,
      port=cfg.server_port,
      reload=False,
      # uvicorn 0.42 applies this to uvicorn.error, uvicorn.access, and
      # uvicorn.asgi, silencing every uvicorn INFO line (access lines,
      # connection open/closed, startup banner); WARNING+ keeps its own format.
      # _RequestLogMiddleware above covers the silenced request lines.
      log_level="warning",
      timeout_graceful_shutdown=timeouts.SERVER_GRACEFUL_SHUTDOWN_TIMEOUT,
  )
