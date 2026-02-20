"""Server-rendered pages — single Jinja2 template for the entire UI."""

from pathlib import Path

import structlog
from fastapi import APIRouter, Depends, Request
from fastapi.responses import HTMLResponse, RedirectResponse
from fastapi.templating import Jinja2Templates

from src.api.deps import get_session_manager, get_thread_manager
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
      # Claude Code events nest text in message.content[].text
      msg = ev.get("message") or {}
      blocks = msg.get("content") or []
      for block in blocks:
        if isinstance(block, dict) and block.get("type") == "text":
          assistant_buf += block.get("text", "")
    elif t == "master_done":
      if assistant_buf:
        messages.append({"role": "assistant", "content": assistant_buf})
        assistant_buf = ""
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
      messages.append({"role": "system", "content": ev.get("content", "")})

  # Flush trailing assistant content (if stream was interrupted)
  if assistant_buf:
    messages.append({"role": "assistant", "content": assistant_buf})

  return messages


@router.get("/", response_class=HTMLResponse)
async def index(
  request: Request,
  session: str | None = None,
  session_mgr: SessionManager = Depends(get_session_manager),
  thread_mgr: ThreadManager = Depends(get_thread_manager),
):
  """Render the full page. All data loaded here."""
  sessions = await session_mgr.list_sessions(status=SessionStatus.ACTIVE)
  active_session = None
  messages: list[dict] = []
  threads = []
  if session:
    active_session = await session_mgr.get_session(session)
    if active_session:
      # Load chat messages from events
      raw_events = session_mgr.load_chat_events_sync(session)
      messages = _events_to_messages(raw_events)

      # Load threads
      threads = await thread_mgr.list_threads(session)

      # Mark session as read
      await session_mgr.mark_read(session)
  elif sessions:
    return RedirectResponse(f"/?session={sessions[0].id}")

  return templates.TemplateResponse("index.html", {
    "request": request,
    "sessions": sessions,
    "active_session": active_session,
    "messages": messages,
    "threads": threads,
  })
