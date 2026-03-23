"""Internal API endpoints — used by master CC to delegate tasks."""

import structlog
from fastapi import APIRouter, Depends, HTTPException

from src.api.deps import get_session_manager, get_thread_manager
from src.core.config import get_config
from src.core.improve_command import ImproveState, run_improve_loop, save_improve_state
from src.core.models import DelegateRequest, ImproveRequest
from src.core.sessions import SessionManager
from src.core.spawner import resolve_session_subagent_backend_model, spawn_worker
from src.core.tasks import create_logged_task
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
  thread = await thread_mgr.create_thread(meta, req.description, context=req.context, require_review=req.require_review)

  # Resolve backend/model from session config before spawning
  cfg = get_config()
  resolved_backend, resolved_model = await resolve_session_subagent_backend_model(req.session_id, cfg, session_mgr)

  # Fire-and-forget: spawn worker in background
  create_logged_task(
      spawn_worker(
          req.session_id,
          req.description,
          thread.id,
          cfg,
          session_mgr,
          thread_mgr,
          repo_path=req.repo_path,
          context=req.context,
          resolved_backend=resolved_backend,
          resolved_model=resolved_model,
      ))

  # Save and broadcast task_delegated event so cursor stays in sync on reconnect
  task_event = {
      "type": "task_delegated",
      "thread_id": thread.id,
      "description": req.description,
      "timestamp": thread.created_at.isoformat(),
      "backend": resolved_backend or "",
      "model": resolved_model or "",
  }
  await session_mgr.persist_and_broadcast(req.session_id, task_event)

  log.info("task_delegated_internal", session=req.session_id, thread_id=thread.id)

  return {
      "thread_id": thread.id,
      "description": req.description,
  }


@router.post("/improve")
async def start_improve_loop(
    req: ImproveRequest,
    session_mgr: SessionManager = Depends(get_session_manager),
    thread_mgr: ThreadManager = Depends(get_thread_manager),
):
  """Launch an iterative improvement loop as a background task."""
  meta = await session_mgr.get_session(req.session_id)
  if not meta:
    raise HTTPException(status_code=404, detail="Session not found")

  cfg = get_config()
  state = ImproveState(goal=req.goal, max_iterations=req.iterations, status='running')
  save_improve_state(req.session_id, state, cfg)

  create_logged_task(
      run_improve_loop(
          session_id=req.session_id,
          repo_path=req.repo_path,
          iterations=req.iterations,
          goal=req.goal,
          cfg=cfg,
          session_mgr=session_mgr,
          thread_mgr=thread_mgr,
      ),
      name=f"improve-loop-{req.session_id}",
  )

  log.info("improve_loop_started", session=req.session_id, iterations=req.iterations, goal=req.goal)

  return {"status": "started", "session_id": req.session_id, "iterations": req.iterations}
