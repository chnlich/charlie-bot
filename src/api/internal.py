"""Internal API endpoints — used by master CC to delegate tasks."""

import asyncio
import time

import structlog
from fastapi import APIRouter, Depends, HTTPException

from src.api.deps import get_session_manager, get_thread_manager, get_trigger_manager
from src.core import event_types as ET
from src.core.config import get_config
from src.core.improve_command import ImproveLoopAlreadyRunningError, reserve_loop_state, run_improve_loop
from src.core.models import DelegateRequest, ImproveRequest, ScheduleTriggerRequest, SpawnRequest
from src.core.sessions import SessionManager
from src.core.spawner import (
    DelegationBlockedError,
    _check_takeoff_gate,
    resolve_requested_subagent_backend_model,
    spawn_worker,
)
from src.core.tasks import create_logged_task
from src.core.threads import ThreadManager
from src.core.triggers import TriggerManager

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

  # Takeoff gate: block delegation unless the user explicitly approved.
  try:
    await asyncio.to_thread(_check_takeoff_gate, req.session_id, session_mgr)
  except DelegationBlockedError as e:
    raise HTTPException(status_code=403, detail=str(e))

  # Resolve backend/model from session config before spawning
  cfg = get_config()
  try:
    resolved_backend, resolved_model = await resolve_requested_subagent_backend_model(
        req.session_id, cfg, session_mgr, requested_backend=req.backend)
  except ValueError as e:
    raise HTTPException(status_code=400, detail=str(e)) from e

  # Create thread immediately so it's visible in the UI
  thread = await thread_mgr.create_thread(meta, req.description, context=req.context, require_review=req.require_review)

  # Fire-and-forget: spawn worker in background
  create_logged_task(
      spawn_worker(
          req.session_id,
          req.description,
          thread.id,
          cfg,
          session_mgr,
          thread_mgr,
          request=SpawnRequest(
              repo_path=req.repo_path,
              base_branch=req.base_branch,
              context=req.context,
              resolved_backend=resolved_backend,
              resolved_model=resolved_model,
              require_takeoff=True,
              keep_worktree=req.keep_worktree,
          ),
      ))

  # Save and broadcast task_delegated event so cursor stays in sync on reconnect
  task_event = {
      "type": ET.TASK_DELEGATED,
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

  try:
    await asyncio.to_thread(_check_takeoff_gate, req.session_id, session_mgr)
  except DelegationBlockedError as e:
    raise HTTPException(status_code=403, detail=str(e))

  cfg = get_config()
  try:
    resolved_backend, resolved_model = await resolve_requested_subagent_backend_model(
        req.session_id, cfg, session_mgr, requested_backend=req.backend)
  except ValueError as e:
    raise HTTPException(status_code=400, detail=str(e)) from e

  work_branch = req.work_branch or req.branch_prefix or f"improve/{int(time.time())}"
  try:
    state = await reserve_loop_state(
        req.session_id,
        req.goal,
        req.iterations,
        work_branch,
        req.repo_path,
        cfg,
        base_branch=req.base_branch,
        merge_back=req.merge_back,
        resolved_backend=resolved_backend,
        resolved_model=resolved_model,
    )
  except ImproveLoopAlreadyRunningError as e:
    raise HTTPException(status_code=409, detail=str(e)) from e

  create_logged_task(
      run_improve_loop(
          session_id=req.session_id,
          repo_path=req.repo_path,
          iterations=req.iterations,
          goal=req.goal,
          cfg=cfg,
          session_mgr=session_mgr,
          thread_mgr=thread_mgr,
          base_branch=req.base_branch,
          work_branch=work_branch,
          merge_back=req.merge_back,
          resolved_backend=resolved_backend,
          resolved_model=resolved_model,
          loop_id=state.loop_id,
      ),
      name=f"improve-loop-{req.session_id}",
  )

  log.info("improve_loop_started", session=req.session_id, iterations=req.iterations, goal=req.goal)

  return {"status": "started", "session_id": req.session_id, "iterations": req.iterations}


@router.post("/schedule-trigger")
async def schedule_trigger(
    req: ScheduleTriggerRequest,
    session_mgr: SessionManager = Depends(get_session_manager),
    trigger_mgr: TriggerManager = Depends(get_trigger_manager),
):
  """Schedule a delayed trigger that will wake the master CC after a delay."""
  meta = await session_mgr.get_session(req.session_id)
  if not meta:
    raise HTTPException(status_code=404, detail="Session not found")

  if req.watch_pids is not None:
    if len(req.watch_pids) == 0:
      raise HTTPException(status_code=400, detail="watch_pids must be non-empty when provided")
    if not all(isinstance(p, int) and p > 0 for p in req.watch_pids):
      raise HTTPException(status_code=400, detail="watch_pids must contain only positive integers")

  try:
    trigger = await trigger_mgr.create_trigger(
        req.session_id,
        req.delay_seconds,
        req.message,
        watch_pids=req.watch_pids,
    )
  except RuntimeError as e:
    raise HTTPException(status_code=400, detail=str(e)) from e
  log.info(
      "trigger_scheduled",
      session=req.session_id,
      trigger_id=trigger.id,
      watch_pids=req.watch_pids,
  )

  return {"trigger_id": trigger.id, "fire_at": trigger.fire_at.isoformat()}


@router.post("/triggers/{session_id}/{trigger_id}/cancel")
async def cancel_trigger(
    session_id: str,
    trigger_id: str,
    trigger_mgr: TriggerManager = Depends(get_trigger_manager),
):
  """Cancel a pending trigger."""
  try:
    await trigger_mgr.cancel_trigger(session_id, trigger_id)
  except FileNotFoundError as exc:
    raise HTTPException(status_code=404, detail="Trigger not found") from exc
  return {"ok": True}
