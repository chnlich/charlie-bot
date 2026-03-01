"""Server-rendered pages — single Jinja2 template for the entire UI."""

import asyncio
from pathlib import Path

import structlog
from fastapi import APIRouter, Depends, HTTPException, Request
from fastapi.responses import HTMLResponse, RedirectResponse
from fastapi.templating import Jinja2Templates

from src.api.deps import get_session_manager, get_thread_manager
from src.api.message_utils import events_to_messages
from src.core.config import CharlieBotConfig, get_config
from src.core.models import SessionStatus
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager

log = structlog.get_logger()

router = APIRouter()
templates = Jinja2Templates(directory=str(Path(__file__).parent.parent.parent / "web" / "templates"))


@router.get("/sessions/{session_id}/events", response_class=HTMLResponse)
async def events_viewer(
    request: Request,
    session_id: str,
    session_mgr: SessionManager = Depends(get_session_manager),
):
  """Render the JSONL events viewer page for a session."""
  try:
    session = await session_mgr.get_session(session_id)
  except Exception:
    log.exception("get_session_failed", session_id=session_id)
    session = None

  if not session:
    raise HTTPException(status_code=404, detail="Session not found")

  return templates.TemplateResponse(
      "events_viewer.html", {
          "request": request,
          "session": session,
          "session_id": session_id,
          "events_url": f"/api/sessions/{session_id}/events.jsonl",
      })


@router.get("/", response_class=HTMLResponse)
async def index(
    request: Request,
    session: str | None = None,
    session_mgr: SessionManager = Depends(get_session_manager),
    thread_mgr: ThreadManager = Depends(get_thread_manager),
    cfg: CharlieBotConfig = Depends(get_config),
):
  """Render the full page. All data loaded here."""
  try:
    sessions = await session_mgr.list_sessions(status=SessionStatus.ACTIVE, scheduled=False)
  except Exception:
    log.exception("list_sessions_failed")
    sessions = []

  active_session = None
  messages: list[dict] = []
  threads = []
  raw_events: list[dict] = []
  session_usage = None
  if session:
    try:
      active_session = await session_mgr.get_session(session)
    except Exception:
      log.exception("get_session_failed", session_id=session)

    if active_session:
      # Load events + threads in parallel; derive usage from already-loaded events
      events_task = asyncio.to_thread(session_mgr.load_chat_events_sync, session)
      threads_task = thread_mgr.list_threads(session)
      try:
        raw_events, threads = await asyncio.gather(events_task, threads_task)
        messages = events_to_messages(raw_events)
        session_usage = session_mgr.usage_from_events(raw_events)
      except Exception:
        log.exception("load_session_data_failed", session_id=session)

      # Mark session as read (fire-and-forget, don't block response)
      try:
        await session_mgr.mark_read(session)
      except Exception:
        log.exception("mark_read_failed", session_id=session)
  elif session is None and sessions:
    return RedirectResponse(f"/?session={sessions[0].id}")

  active_backend = active_session.backend if active_session else (
      cfg.backend_options[0].id if cfg.backend_options else "claude-opus-4.6")
  active_backend_label = next(
      (opt.label for opt in cfg.backend_options if opt.id == active_backend),
      active_backend)

  return templates.TemplateResponse(
      "index.html", {
          "request": request,
          "sessions": sessions,
          "active_session": active_session,
          "messages": messages,
          "threads": threads,
          "event_count": len(raw_events),
          "session_usage": session_usage,
          "all_usage": {},
          "backend_options": cfg.backend_options,
          "active_backend": active_backend,
          "active_backend_label": active_backend_label,
      })
