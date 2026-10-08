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
    get_session_events,
    get_session_listing,
    get_session_store,
    get_task_manager,
)
from src.runtime.api.message_utils import build_session_bootstrap_data
from src.runtime.api.sessions import (
    _bootstrap_payload,
    _default_backend_id,
    apply_listing_fields,
)
from src.runtime.hooks import page_render
from src.runtime.hooks.sequence_controllers import sequence_listing_fields
from src.runtime.session_events import SessionEvents
from src.runtime.session_listing import SessionListing
from src.runtime.session_store import SessionStore
from src.runtime.task_sessions import TaskTreeManager

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
    store: SessionStore = Depends(get_session_store),
) -> HTMLResponse:
  """Render the JSONL events viewer page for a session."""
  try:
    session = await store.get_session(session_id)
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
    listing: SessionListing = Depends(get_session_listing),
    store: SessionStore = Depends(get_session_store),
    session_events: SessionEvents = Depends(get_session_events),
    cfg: CharlieBotConfig = Depends(get_config_on_loop),
    task_mgr: TaskTreeManager = Depends(get_task_manager),
) -> Response:
  """Render the full page with only critical active-session data.

  ``/?session=<id>`` opens a task node (a worker node's messages are its Runs'
  transcript).
  """
  load_errors: list[str] = []
  try:
    sessions = await listing.list_sessions(
        status=SessionStatus.ACTIVE,
        scheduled=False,
        include_running_status=True,
        include_pending_trigger_status=True,
    )
    # The first-paint list shares the All endpoint's membership: sequence-subtree
    # rows and every sidebar view's subtree ride no listing, so a firing leaf neither
    # flattens into a top-level sidebar row, a chat-platform thread session
    # never paints into Workspace, and neither becomes the auto-redirect target.
    sequence_subtree = await listing.sequence_subtree_roots()
    views = await listing.view_subtree_roots()
    sessions = [
        session for session in sessions
        if session.id not in sequence_subtree and not any(session.id in view for view in views.values())
    ]
  except Exception:
    log.exception("list_sessions_failed")
    sessions = []
    load_errors.append("Failed to load sessions. Check server logs for details.")

  active_session = None
  pending_draft: dict | None = None
  event_count = 0
  session_bootstrap: dict | None = None
  if session:
    try:
      active_session = await store.get_session(session)
    except Exception:
      log.exception("get_session_failed", session_id=session)

    if active_session:
      try:
        bootstrap = await build_session_bootstrap_data(session, store, session_events, tree=task_mgr)
        active_session = bootstrap.session
        pending_draft = bootstrap.pending_draft
        event_count = bootstrap.total_event_count
        for sidebar_session in sessions:
          if sidebar_session.id == session:
            sidebar_session.has_unread = False
        session_bootstrap = _bootstrap_payload(bootstrap, cfg)
      except Exception:
        log.exception("load_session_data_failed", session_id=session)
        load_errors.append("Failed to load session data. Check server logs for details.")
  elif session is None and sessions:
    return RedirectResponse(f"/?session={sessions[0].id}")

  listing_fields = sequence_listing_fields((s.id for s in sessions), dt.datetime.now(dt.UTC))
  initial_sessions = [apply_listing_fields(s.model_dump(mode="json"), listing_fields[s.id]) for s in sessions]

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
          "pending_draft": pending_draft,
          "event_count": event_count,
          "session_bootstrap": session_bootstrap,
          "backend_options": cfg.backends.options,
          "active_backend": active_backend,
          "active_backend_label": active_backend_label,
          "active_backend_type": active_backend_type,
          "load_errors": load_errors,
          "auth_enabled": bool(configured_access_key()),
          "hostname": socket.gethostname(),
          "sessions_root": str(cfg.sessions_dir),
          "version": templating.git_version(),
          "static_asset_version": templating.static_asset_version(),
      })
