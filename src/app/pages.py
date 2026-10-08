"""Server-rendered pages — one Jinja2 template per page under web/templates/: the chat
UI, home and events viewer. A feature package brings its own pages and templates through
the page-render registry (src.runtime.hooks.page_render)."""

import asyncio
import datetime as dt
import socket
from urllib.parse import urlparse

from fastapi import APIRouter, Depends, HTTPException, Request
from fastapi.responses import HTMLResponse, JSONResponse, RedirectResponse
from starlette.responses import Response

from src.infra.config import CharlieBotConfig, configured_access_key
from src.infra.constants import AUTH_STATUS_PATH
from src.infra.log_once import LazyStructlogLogger
from src.infra.models import SessionStatus
from src.infra.timeouts import HOME_SERVICE_PROBE_TIMEOUT
from src.runtime import templating
from src.runtime.api.deps import (
    SESSION_NOT_FOUND_DETAIL,
    get_config_on_loop,
    get_session_manager,
    get_task_manager,
    get_thread_manager,
)
from src.runtime.api.message_utils import build_session_bootstrap_data
from src.runtime.api.sessions import (
    _bootstrap_payload,
    _default_backend_id,
    apply_row_schedule,
    project_worker_threads,
    row_schedule_fields,
)
from src.runtime.hooks import page_render
from src.runtime.sessions import SessionManager
from src.runtime.task_sessions import TaskTreeManager
from src.runtime.threads import ThreadManager

log = LazyStructlogLogger()

# The app's own destinations, listed first on the home page; feature packages register
# the cards that follow (page_render.register_home_card). The same on every host, so they
# live in code rather than config; each renders as a card linking straight to the page.
_HOME_DESTINATIONS = ({"name": "Chat", "url": "/", "description": "The CharlieBot chat and session UI."},)


def _probe_home_service(url: str) -> bool:
  """TCP-connect to the host and port parsed out of *url*; True when the connect succeeds.

  The port defaults by scheme (443 for https, 80 for http). Any failure — refused or
  timed-out connect, an unparseable or out-of-range port, or a URL with no host —
  returns False; a probe never raises out of the home route.
  """
  try:
    parsed = urlparse(url)
    host = parsed.hostname
    port = parsed.port
  except ValueError:
    return False
  if not host:
    return False
  if port is None:
    port = 443 if parsed.scheme == "https" else 80
  try:
    with socket.create_connection((host, port), timeout=HOME_SERVICE_PROBE_TIMEOUT):
      return True
  except OSError:
    return False


router = APIRouter()


@router.get(AUTH_STATUS_PATH)
async def auth_status() -> JSONResponse:
  """Return whether access-key authentication is enabled."""
  return JSONResponse({"auth_enabled": bool(configured_access_key())})


@router.get("/sessions/{session_id}/events", response_class=HTMLResponse)
async def events_viewer(
    request: Request,
    session_id: str,
    session_mgr: SessionManager = Depends(get_session_manager),
) -> HTMLResponse:
  """Render the JSONL events viewer page for a session."""
  try:
    session = await session_mgr.get_session(session_id)
  except (KeyError, FileNotFoundError) as e:
    raise HTTPException(status_code=404, detail=SESSION_NOT_FOUND_DETAIL) from e
  except Exception as e:
    log.exception("get_session_failed", session_id=session_id)
    raise HTTPException(status_code=500, detail="Failed to load session") from e

  if not session:
    raise HTTPException(status_code=404, detail=SESSION_NOT_FOUND_DETAIL)

  return templating.templates().TemplateResponse(
      request,
      "events_viewer.html",
      context={
          "session": session,
          "session_id": session_id,
          "events_url": f"/api/sessions/{session_id}/events.jsonl",
          "hostname": socket.gethostname(),
          "static_asset_version": templating.static_asset_version(),
      })


@router.get("/home", response_class=HTMLResponse)
async def home_page(request: Request, cfg: CharlieBotConfig = Depends(get_config_on_loop)) -> HTMLResponse:
  """Render the home page: this server's destinations plus the per-host external services.

  Each external service is probed by TCP-connecting to the host and port parsed out of
  its configured ``url`` — the same address its card links to. Probes run off the event
  loop, concurrently, and nothing is persisted or cached.
  """
  statuses = await asyncio.gather(
      *(asyncio.to_thread(_probe_home_service, service.url) for service in cfg.ui.home_services))
  services = [
      {
          "name": service.name,
          "description": service.description,
          "url": service.url,
          "status": "up" if up else "down",
      } for service, up in zip(cfg.ui.home_services, statuses, strict=True)
  ]
  return templating.templates().TemplateResponse(
      request,
      "home.html",
      context={
          "hostname": socket.gethostname(),
          "destinations": (*_HOME_DESTINATIONS, *page_render.home_cards()),
          "services": services,
      })


@router.get("/", response_class=HTMLResponse)
async def index(
    request: Request,
    session: str | None = None,
    thread: str | None = None,
    session_mgr: SessionManager = Depends(get_session_manager),
    cfg: CharlieBotConfig = Depends(get_config_on_loop),
    task_mgr: TaskTreeManager = Depends(get_task_manager),
    thread_mgr: ThreadManager = Depends(get_thread_manager),
) -> Response:
  """Render the full page with only critical active-session data.

  ``/?session=<id>`` opens a session (a worker node's messages are its Runs'
  transcript); ``/?session=<parent>&thread=<id>`` opens one legacy worker
  thread projected into the same main-chat view, read-only.
  """
  # The M99 import floor carries no speech stack (the M99 row's rule) and no
  # preview probe; both serve only this page's context build.
  from src.features.session_tree_preview.session_tree_preview import is_preview_mode
  from src.features.voice.transcription.registry import build_transcription_backends
  load_errors: list[str] = []
  try:
    sessions = await session_mgr.list_sessions(
        status=SessionStatus.ACTIVE,
        scheduled=False,
        include_running_status=True,
        include_pending_trigger_status=True,
    )
    # The first-paint list shares the All endpoint's membership: cron-subtree
    # rows and the chat-thread subtree ride no listing, so a firing leaf neither
    # flattens into a top-level sidebar row, a Slack/Discord thread session
    # never paints into Workspace, and neither becomes the auto-redirect target.
    cron_subtree = await session_mgr.cron_subtree_roots()
    chat_threads = await session_mgr.chat_thread_subtree_roots()
    sessions = [s for s in sessions if s.id not in cron_subtree and s.id not in chat_threads]
  except Exception:
    log.exception("list_sessions_failed")
    sessions = []
    load_errors.append("Failed to load sessions. Check server logs for details.")

  active_session = None
  pending_draft: dict | None = None
  event_count = 0
  session_bootstrap: dict | None = None
  thread_view: dict | None = None
  thread_thinking = None
  if session:
    try:
      active_session = await session_mgr.get_session(session)
    except Exception:
      log.exception("get_session_failed", session_id=session)

    if active_session and thread:
      # The legacy thread view: the page renders the parent session's chrome
      # (sidebar highlight, status poll) over the thread's projected
      # transcript, read-only, addressed by this URL.
      thread_meta = await thread_mgr.get_thread(session, thread)
      if thread_meta is None:
        load_errors.append(f"Thread {thread} not found in session {session}.")
      else:
        thread_view = {
            "session_id": session,
            "thread_id": thread,
            "description": thread_meta.description,
            "backend": thread_meta.backend or "",
        }
    if active_session:
      try:
        bootstrap = await build_session_bootstrap_data(session, session_mgr, tree=task_mgr)
        active_session = bootstrap.session
        pending_draft = bootstrap.pending_draft
        event_count = bootstrap.total_event_count
        for sidebar_session in sessions:
          if sidebar_session.id == session:
            sidebar_session.has_unread = False
        session_bootstrap = _bootstrap_payload(bootstrap, cfg)
        if thread_view is not None:
          # The header names the thread, not the parent session; the parent
          # session metadata only addresses the view.
          from src.runtime import worker_transcript
          entry = await asyncio.to_thread(
              worker_transcript.load_thread_transcript, cfg, cfg.sessions_dir / session, await
              thread_mgr.get_thread(session, thread), await thread_mgr.get_events_log_path(session, thread))
          session_bootstrap = {
              **session_bootstrap,
              "session":
                  {
                      **session_bootstrap["session"], "name": thread_view["description"],
                      "profile": "worker",
                      "backend": thread_view["backend"]
                  },
              "messages":
                  [m.model_dump(mode="json") if hasattr(m, "model_dump") else m for m in entry.projection.committed],
              "pending_draft": entry.projection.pending_draft,
              "event_count": entry.projection.event_count,
              "oldest_message_ordinal": 0,
              "has_more": False,
              "thread_view": thread_view,
          }
          thread_thinking = worker_transcript.thread_thinking_since(thread_meta)
      except Exception:
        log.exception("load_session_data_failed", session_id=session)
        load_errors.append("Failed to load session data. Check server logs for details.")
  elif session is None and sessions:
    return RedirectResponse(f"/?session={sessions[0].id}")

  # The first-paint sidebar list carries the legacy worker-thread leaves too;
  # projected after the redirect check so a thread row can never become the
  # auto-redirect target. Row shape matches GET /api/sessions/: the schedule
  # join stamps every row, so the first paint shows a scheduled node's clock.
  sessions = await project_worker_threads(sessions, cfg, thread_mgr)
  schedule_fields = row_schedule_fields((s.id for s in sessions), dt.datetime.now(dt.UTC))
  initial_sessions = [apply_row_schedule(s.model_dump(mode="json"), schedule_fields[s.id]) for s in sessions]

  if thread_view is not None:
    active_backend = thread_view.get("backend") or _default_backend_id(cfg)
  else:
    active_backend = (
        (active_session.run_backend or active_session.backend) if active_session else _default_backend_id(cfg))
  active_backend_opt = cfg.get_backend_option(active_backend)
  active_backend_label = active_backend_opt.label if active_backend_opt else active_backend
  active_backend_type = active_backend_opt.type if active_backend_opt else ""

  return templating.templates().TemplateResponse(
      request,
      "index.html",
      context={
          "initial_sessions": initial_sessions,
          "active_session": active_session,
          "thread_view": thread_view,
          "thread_thinking": thread_thinking,
          "pending_draft": pending_draft,
          "event_count": event_count,
          "session_bootstrap": session_bootstrap,
          "backend_options": cfg.backends.options,
          "voice_backends":
              [
                  {
                      "id": backend.id,
                      "label": backend.label,
                      "live_partials": backend.live_partials,
                      "unavailable_reason": backend.unavailable_reason(),
                  } for backend in build_transcription_backends(cfg)
              ],
          "voice_default_backend": cfg.voice.default_backend,
          "active_backend": active_backend,
          "active_backend_label": active_backend_label,
          "active_backend_type": active_backend_type,
          "load_errors": load_errors,
          "auth_enabled": bool(configured_access_key()),
          "hostname": socket.gethostname(),
          "sessions_root": str(cfg.sessions_dir),
          "version": templating.git_version(),
          "static_asset_version": templating.static_asset_version(),
          "preview_mode": is_preview_mode(),
      })
