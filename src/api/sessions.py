"""Session management API routes."""

from fastapi import APIRouter, Depends, HTTPException

from src.core.config import get_config
from src.core.models import (
  CreateSessionRequest,
  Priority,
  RenameSessionRequest,
  ReorderTaskRequest,
  SessionMetadata,
  TaskQueue,
  ThreadMetadata,
)
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager
from src.api.deps import get_session_manager, get_thread_manager, get_queue_manager
from src.core.queue import QueueManager

router = APIRouter()


@router.get("/", response_model=list[SessionMetadata])
async def list_sessions(session_mgr: SessionManager = Depends(get_session_manager)):
  return await session_mgr.list_sessions()


@router.post("/", response_model=SessionMetadata)
async def create_session(
  req: CreateSessionRequest,
  session_mgr: SessionManager = Depends(get_session_manager),
):
  return await session_mgr.create_session(req)


@router.get("/projects")
async def list_projects():
  """Return git repos discovered from configured workspace_dirs."""
  cfg = get_config()
  return cfg.discover_repos()


@router.get("/{session_id}", response_model=SessionMetadata)
async def get_session(session_id: str, session_mgr: SessionManager = Depends(get_session_manager)):
  meta = await session_mgr.get_session(session_id)
  if not meta:
    raise HTTPException(status_code=404, detail="Session not found")
  return meta


@router.delete("/{session_id}")
async def archive_session(session_id: str, session_mgr: SessionManager = Depends(get_session_manager)):
  meta = await session_mgr.get_session(session_id)
  if not meta:
    raise HTTPException(status_code=404, detail="Session not found")
  await session_mgr.archive_session(session_id)
  return {"ok": True}


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


@router.get("/{session_id}/threads", response_model=list[ThreadMetadata])
async def list_threads(session_id: str, thread_mgr: ThreadManager = Depends(get_thread_manager)):
  return await thread_mgr.list_threads(session_id)


@router.get("/{session_id}/queue", response_model=TaskQueue)
async def get_queue(session_id: str, queue_mgr: QueueManager = Depends(get_queue_manager)):
  return await queue_mgr.get_queue()


@router.post("/{session_id}/queue/reorder")
async def reorder_task(
  session_id: str,
  req: ReorderTaskRequest,
  queue_mgr: QueueManager = Depends(get_queue_manager),
):
  await queue_mgr.reorder(req.task_id, req.priority)
  return {"ok": True}


@router.delete("/{session_id}/queue/{task_id}")
async def cancel_task(
  session_id: str,
  task_id: str,
  queue_mgr: QueueManager = Depends(get_queue_manager),
):
  await queue_mgr.cancel(task_id)
  return {"ok": True}
