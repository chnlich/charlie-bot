"""Server-rendered pages — single Jinja2 template for the entire UI."""

from pathlib import Path

import structlog
from fastapi import APIRouter, Depends, HTTPException, Request
from fastapi.responses import HTMLResponse, RedirectResponse
from fastapi.templating import Jinja2Templates

from src.api.deps import get_session_manager, get_thread_manager
from src.core.config import CharlieBotConfig, get_config
from src.core.models import SessionStatus
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager

log = structlog.get_logger()

router = APIRouter()
templates = Jinja2Templates(directory=str(Path(__file__).parent.parent.parent / "web" / "templates"))


def _events_to_messages(events: list[dict]) -> list[dict]:
  """Convert raw chat_events.jsonl entries into displayable messages."""
  messages = []
  assistant_buf = ""

  for ev in events:
    t = ev.get("type")
    if t == "user":
      # Skip CC-internal user events (tool results) — they have a "message" field
      # but no top-level "content". Only real user messages have "content".
      if "message" in ev and "content" not in ev:
        continue
      # Flush any pending assistant buffer
      if assistant_buf:
        messages.append({"role": "assistant", "content": assistant_buf})
        assistant_buf = ""
      messages.append({
          "role": "user",
          "content": ev.get("content", ""),
          "is_voice": ev.get("is_voice", False),
      })
    elif t == "assistant":
      msg = ev.get("message") or {}
      blocks = msg.get("content") or []
      for b in blocks:
        if isinstance(b, dict) and b.get('type') == 'tool_use' and b.get('name') == 'ExitPlanMode':
          plan_text = (b.get('input') or {}).get('plan', '')
          if plan_text:
            if assistant_buf:
              messages.append({'role': 'assistant', 'content': assistant_buf})
              assistant_buf = ''
            messages.append({'role': 'plan', 'content': plan_text})
      text = "".join(b.get("text", "") for b in blocks if isinstance(b, dict) and b.get("type") == "text")
      if text and assistant_buf:
        messages.append({"role": "assistant", "content": assistant_buf})
        assistant_buf = ""
      assistant_buf += text
    elif t == "master_done":
      if assistant_buf:
        messages.append({"role": "assistant", "content": assistant_buf})
        assistant_buf = ""
      if not ev.get("still_thinking"):
        messages.append({"role": "separator", "thinking_seconds": ev.get("thinking_seconds")})
    elif t == "assistant_error":
      if assistant_buf:
        messages.append({"role": "assistant", "content": assistant_buf})
        assistant_buf = ""
      messages.append({"role": "system", "content": f"Error: {ev.get('content', '')}"})
    elif t == "task_delegated":
      if assistant_buf:
        messages.append({"role": "assistant", "content": assistant_buf})
        assistant_buf = ""
      desc = ev.get("description", "")
      messages.append({"role": "system", "content": f"Task delegated: {desc}"})
    elif t == "worker_summary":
      if assistant_buf:
        messages.append({"role": "assistant", "content": assistant_buf})
        assistant_buf = ""
      messages.append({"role": "worker_summary", "content": ev.get("content", "")})

  # Flush trailing assistant content (if stream was interrupted)
  if assistant_buf:
    messages.append({"role": "assistant", "content": assistant_buf})

  return messages


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
    sessions = await session_mgr.list_sessions(status=SessionStatus.ACTIVE)
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
      # Load chat messages from events
      try:
        raw_events = session_mgr.load_chat_events_sync(session)
        messages = _events_to_messages(raw_events)
      except Exception:
        log.exception("load_chat_events_failed", session_id=session)

      # Load threads
      try:
        threads = await thread_mgr.list_threads(session)
      except Exception:
        log.exception("list_threads_failed", session_id=session)

      # Load token usage for active session
      try:
        session_usage = session_mgr.get_session_usage(session)
      except Exception:
        log.exception("get_session_usage_failed", session_id=session)

      # Mark session as read
      try:
        await session_mgr.mark_read(session)
      except Exception:
        log.exception("mark_read_failed", session_id=session)
  elif session is None and sessions:
    return RedirectResponse(f"/?session={sessions[0].id}")

  # Compute usage for each sidebar session
  all_usage: dict[str, dict] = {}
  for s in sessions:
    try:
      u = session_mgr.get_session_usage(s.id)
      if u:
        all_usage[s.id] = u
    except Exception:
      log.debug("sidebar_usage_failed", session_id=s.id)

  active_backend = active_session.backend if active_session else (
      cfg.backend_options[0].id if cfg.backend_options else "claude-opus-4.6")

  return templates.TemplateResponse(
      "index.html", {
          "request": request,
          "sessions": sessions,
          "active_session": active_session,
          "messages": messages,
          "threads": threads,
          "event_count": len(raw_events),
          "session_usage": session_usage,
          "all_usage": all_usage,
          "backend_options": cfg.backend_options,
          "active_backend": active_backend,
      })
