"""Internal API endpoints — used by master CC to delegate tasks."""

import structlog
from fastapi import APIRouter, Depends, HTTPException

from src.api.deps import get_session_manager, get_thread_manager
from src.core.config import get_config
from src.core.dispatcher import get_or_create as get_or_create_dispatcher
from src.core.models import DelegateRequest, Task
from src.core.sessions import SessionManager
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
  """Create a task and thread, then enqueue for a worker agent."""
  meta = await session_mgr.get_session(req.session_id)
  if not meta:
    raise HTTPException(status_code=404, detail="Session not found")

  task = Task(
    priority=req.priority,
    description=req.description,
    is_plan_mode=req.plan_mode,
  )

  # Create thread immediately so it's visible in the UI
  thread = await thread_mgr.create_thread(meta, task)
  task.thread_id = thread.id

  # Get the dispatcher for this session and enqueue
  cfg = get_config()
  dispatcher = get_or_create_dispatcher(
    req.session_id, cfg, session_mgr, thread_mgr,
  )
  await dispatcher.enqueue(task)

  # Broadcast task_delegated event on the session WebSocket
  await streaming_manager.broadcast(f"session:{req.session_id}", {
    "type": "task_delegated",
    "task_id": task.id,
    "thread_id": thread.id,
    "priority": task.priority.value,
    "description": task.description,
    "plan_mode": task.is_plan_mode,
  })

  log.info("task_delegated_internal", session=req.session_id, task_id=task.id, thread_id=thread.id)

  return {
    "task_id": task.id,
    "thread_id": thread.id,
    "priority": task.priority.value,
    "description": task.description,
  }
