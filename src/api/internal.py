"""Internal API endpoints — used by master CC to delegate tasks."""

import asyncio

import structlog
from fastapi import APIRouter, Depends, HTTPException

from src.api.deps import get_session_manager, get_thread_manager
from src.core.config import get_config
from src.core.models import DelegateRequest
from src.core.sessions import SessionManager
from src.core.spawner import spawn_worker
from src.core.streaming import streaming_manager
from src.core.threads import ThreadManager

log = structlog.get_logger()

router = APIRouter()


@router.post("/delegate")
async def delegate_task(
  req: DelegateRequest,
  session_mgr: SessionManager = Depends(get_session_manager),
  thread_mgr: ThreadManager = Depends(get_thread_manager),
):
  """Create a thread and spawn a worker agent directly."""
  meta = await session_mgr.get_session(req.session_id)
  if not meta:
    raise HTTPException(status_code=404, detail="Session not found")

  # Create thread immediately so it's visible in the UI
  thread = await thread_mgr.create_thread(meta, req.description)

  # Fire-and-forget: spawn worker in background
  cfg = get_config()
  asyncio.create_task(spawn_worker(
    req.session_id, req.description, thread.id,
    cfg, session_mgr, thread_mgr,
    repo_path=req.repo_path,
  ))

  # Broadcast task_delegated event on the session WebSocket
  await streaming_manager.broadcast(f"session:{req.session_id}", {
    "type": "task_delegated",
    "thread_id": thread.id,
    "description": req.description,
  })

  log.info("task_delegated_internal", session=req.session_id, thread_id=thread.id)

  return {
    "thread_id": thread.id,
    "description": req.description,
  }
