"""Session management API routes."""

import asyncio
from datetime import datetime
from zoneinfo import ZoneInfo

import structlog
from croniter import croniter
from fastapi import APIRouter, Depends, HTTPException
from fastapi.responses import FileResponse

from src.api.deps import get_session_manager, get_thread_manager, require_session
from src.api.message_utils import events_to_messages
from src.core.config import CharlieBotConfig, get_config, get_scheduled_tasks
from src.core.models import (
    CreateSessionRequest,
    RenameSessionRequest,
    SessionMetadata,
    SessionStatus,
    ThreadMetadata,
)
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager

log = structlog.get_logger()
router = APIRouter()


@router.get("/", response_model=list[SessionMetadata])
async def list_sessions(session_mgr: SessionManager = Depends(get_session_manager)):
  return await session_mgr.list_sessions(status=SessionStatus.ACTIVE, scheduled=False)


@router.post("/", response_model=SessionMetadata)
async def create_session(
    req: CreateSessionRequest,
    session_mgr: SessionManager = Depends(get_session_manager),
):
  return await session_mgr.create_session(req)


@router.get("/projects")
async def list_projects():
  """Return git repos discovered from configured workspace_dirs."""
  from src.core.config import get_config
  cfg = get_config()
  return await asyncio.to_thread(cfg.discover_repos)


@router.get("/archived", response_model=list[SessionMetadata])
async def list_archived_sessions(session_mgr: SessionManager = Depends(get_session_manager)):
  """List archived sessions, newest first."""
  return await session_mgr.list_sessions(status=SessionStatus.ARCHIVED)


@router.get("/waiting", response_model=list[SessionMetadata])
async def list_waiting_sessions(session_mgr: SessionManager = Depends(get_session_manager)):
  """List waiting sessions, newest first."""
  return await session_mgr.list_sessions(status=SessionStatus.WAITING)


@router.get("/starred", response_model=list[SessionMetadata])
async def list_starred_sessions(session_mgr: SessionManager = Depends(get_session_manager)):
  """List starred sessions, newest first."""
  return await session_mgr.list_sessions(starred=True)


@router.get("/scheduled", response_model=list[SessionMetadata])
async def list_scheduled_sessions(
    session_mgr: SessionManager = Depends(get_session_manager),
    cfg: CharlieBotConfig = Depends(get_config),
):
  """List sessions with a scheduled task, newest first."""
  sessions = await session_mgr.list_sessions(scheduled=True)
  task_map = {t.name: t for t in get_scheduled_tasks()}
  for s in sessions:
    task = task_map.get(s.scheduled_task)
    if task:
      s.schedule_cron = task.cron
      s.schedule_enabled = task.enabled
      s.schedule_timezone = task.timezone
      s.schedule_project = task.project
      s.schedule_allow_failure = task.allow_failure
      tz = ZoneInfo(task.timezone)
      now = datetime.now(tz)
      s.schedule_next_run = croniter(task.cron, now).get_next(datetime).isoformat()
    else:
      s.schedule_enabled = False
  return sessions


@router.get('/usage')
async def all_sessions_usage(session_mgr: SessionManager = Depends(get_session_manager)):
  """Return {session_id: usage_dict} for all active sessions (lazy-loaded by frontend)."""
  session_ids = await asyncio.to_thread(session_mgr.list_active_session_ids)

  async def _fetch(sid: str) -> tuple[str, dict | None]:
    try:
      return sid, await asyncio.to_thread(session_mgr.get_session_usage, sid)
    except Exception:
      return sid, None

  pairs = await asyncio.gather(*[_fetch(sid) for sid in session_ids])
  return {sid: u for sid, u in pairs if u}


@router.get('/search', response_model=list[SessionMetadata])
async def search_sessions(q: str = '', session_mgr: SessionManager = Depends(get_session_manager)):
  """Full-text search across session names and chat content."""
  if not q.strip():
    return await session_mgr.list_sessions(status=SessionStatus.ACTIVE)
  return await session_mgr.search_sessions(q.strip())


@router.get('/{session_id}/view')
async def get_session_view(
    session_id: str,
    session_mgr: SessionManager = Depends(get_session_manager),
    thread_mgr: ThreadManager = Depends(get_thread_manager),
    cfg: CharlieBotConfig = Depends(get_config),
):
  """Return all data needed to render a session chat panel (SPA switch)."""
  meta = await session_mgr.get_session(session_id)
  if not meta:
    raise HTTPException(status_code=404, detail="Session not found")
  events_task = asyncio.to_thread(session_mgr.load_chat_events_sync, session_id)
  threads_task = thread_mgr.list_threads(session_id)
  raw_events, threads = await asyncio.gather(events_task, threads_task)
  messages = events_to_messages(raw_events)
  usage = session_mgr.usage_from_events(raw_events)
  try:
    await session_mgr.mark_read(session_id)
  except Exception:
    log.warning("mark_read_failed", session_id=session_id)
  active_backend = meta.backend or (cfg.backend_options[0].id if cfg.backend_options else "claude-opus-4.6")
  return {
      "session": meta.model_dump(mode="json"),
      "messages": messages,
      "threads": [t.model_dump(mode="json") for t in threads],
      "event_count": len(raw_events),
      "usage": usage,
      "active_backend": active_backend,
  }


@router.post('/{session_id}/rewind', response_model=SessionMetadata)
async def rewind_session(
    session_id: str,
    body: dict,
    session_mgr: SessionManager = Depends(get_session_manager),
):
  """Create a new session by rewinding to a specific event index."""
  event_index = body.get('event_index')
  if event_index is None:
    raise HTTPException(status_code=400, detail='event_index is required')
  meta = await session_mgr.rewind_session(session_id, int(event_index))
  if not meta:
    raise HTTPException(status_code=404, detail='Session not found')
  return meta


@router.get("/{session_id}", response_model=SessionMetadata)
async def get_session(meta: SessionMetadata = Depends(require_session)):
  return meta


@router.delete("/{session_id}", response_model=SessionMetadata)
async def archive_session(session_id: str, session_mgr: SessionManager = Depends(get_session_manager)):
  meta = await session_mgr.archive_session(session_id)
  if not meta:
    raise HTTPException(status_code=404, detail="Session not found")
  return meta


@router.post("/{session_id}/unarchive", response_model=SessionMetadata)
async def unarchive_session(
    session_id: str,
    meta: SessionMetadata = Depends(require_session),
    session_mgr: SessionManager = Depends(get_session_manager),
):
  if meta.status != SessionStatus.ARCHIVED:
    raise HTTPException(status_code=409, detail="Session is not archived")
  return await session_mgr.unarchive_session(session_id)


@router.post("/{session_id}/star", response_model=SessionMetadata)
async def star_session(session_id: str, session_mgr: SessionManager = Depends(get_session_manager)):
  meta = await session_mgr.star_session(session_id)
  if not meta:
    raise HTTPException(status_code=404, detail="Session not found")
  return meta


@router.post("/{session_id}/unstar", response_model=SessionMetadata)
async def unstar_session(session_id: str, session_mgr: SessionManager = Depends(get_session_manager)):
  meta = await session_mgr.unstar_session(session_id)
  if not meta:
    raise HTTPException(status_code=404, detail="Session not found")
  return meta


@router.post("/{session_id}/wait", response_model=SessionMetadata)
async def wait_session(
    session_id: str,
    meta: SessionMetadata = Depends(require_session),
    session_mgr: SessionManager = Depends(get_session_manager),
):
  if meta.status != SessionStatus.ACTIVE:
    raise HTTPException(status_code=409, detail="Session is not active")
  return await session_mgr.mark_waiting(session_id)


@router.post("/{session_id}/unwait", response_model=SessionMetadata)
async def unwait_session(
    session_id: str,
    meta: SessionMetadata = Depends(require_session),
    session_mgr: SessionManager = Depends(get_session_manager),
):
  if meta.status != SessionStatus.WAITING:
    raise HTTPException(status_code=409, detail="Session is not waiting")
  return await session_mgr.unmark_waiting(session_id)


@router.patch("/{session_id}", response_model=SessionMetadata)
async def rename_session(
    session_id: str,
    req: RenameSessionRequest,
    session_mgr: SessionManager = Depends(get_session_manager),
):
  meta = await session_mgr.rename_session(session_id, req.name)
  if not meta:
    raise HTTPException(status_code=404, detail="Session not found")
  return meta


@router.post("/{session_id}/read")
async def mark_session_read(session_id: str, session_mgr: SessionManager = Depends(get_session_manager)):
  meta = await session_mgr.mark_read(session_id)
  if not meta:
    raise HTTPException(status_code=404, detail="Session not found")
  return {"ok": True}


@router.get("/{session_id}/events.jsonl")
async def get_events_jsonl(session_id: str):
  """Serve the raw chat_events.jsonl file for a session."""
  from src.core.config import get_config
  cfg = get_config()
  path = cfg.sessions_dir / session_id / "data" / "chat_events.jsonl"
  if not path.exists():
    raise HTTPException(status_code=404, detail="Events file not found")
  return FileResponse(path, media_type="application/x-ndjson")


@router.get("/{session_id}/threads", response_model=list[ThreadMetadata])
async def list_threads(session_id: str, thread_mgr: ThreadManager = Depends(get_thread_manager)):
  return await thread_mgr.list_threads(session_id)
