"""Session management API routes."""

from fastapi import APIRouter, Depends, HTTPException
from fastapi.responses import FileResponse

from src.core.models import (
  CreateSessionRequest,
  RenameSessionRequest,
  SessionMetadata,
  SessionStatus,
  ThreadMetadata,
)
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager
from src.api.deps import get_session_manager, get_thread_manager, require_session

router = APIRouter()


@router.get("/", response_model=list[SessionMetadata])
async def list_sessions(session_mgr: SessionManager = Depends(get_session_manager)):
  return await session_mgr.list_sessions(status=SessionStatus.ACTIVE)


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
  return cfg.discover_repos()


@router.get("/archived", response_model=list[SessionMetadata])
async def list_archived_sessions(session_mgr: SessionManager = Depends(get_session_manager)):
  """List archived sessions, newest first."""
  return await session_mgr.list_sessions(status=SessionStatus.ARCHIVED)


@router.get("/starred", response_model=list[SessionMetadata])
async def list_starred_sessions(session_mgr: SessionManager = Depends(get_session_manager)):
  """List starred sessions, newest first."""
  return await session_mgr.list_sessions(starred=True)


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
