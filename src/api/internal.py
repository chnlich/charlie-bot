"""Internal API endpoints — used by master CC to delegate tasks."""

import asyncio
import time
from typing import Union

import structlog
from fastapi import APIRouter, Depends, HTTPException

from src.api.deps import get_session_manager, get_thread_manager, get_trigger_manager
from src.core import event_types as ET
from src.core.config import CharlieBotConfig, get_config
from src.core.improve_command import ImproveLoopAlreadyRunningError, reserve_loop_state, run_improve_loop
from src.core.models import (
    DelegateRequest,
    ImproveRequest,
    ScheduleTriggerRequest,
    SessionMetadata,
    SpawnRequest,
    TaskType,
    WatchTarget,
)
from src.core.sessions import SessionManager
from src.core.spawner import (
    DelegationBlockedError,
    _check_takeoff_gate,
    resolve_requested_subagent_backend_model,
    spawn_worker,
)
from src.core.tasks import create_logged_task
from src.core.threads import ThreadManager
from src.core.triggers import RemoteVerifyError, TriggerManager

log = structlog.get_logger()

router = APIRouter()


async def _authorize_spawn_request(
    req: Union[DelegateRequest, ImproveRequest],
    session_mgr: SessionManager,
) -> tuple[SessionMetadata, CharlieBotConfig, str | None, str | None]:
  """Validate session, enforce takeoff gate, and resolve backend/model for spawn-style endpoints."""
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

  return meta, cfg, resolved_backend, resolved_model


@router.post("/delegate")
async def delegate_task(
    req: DelegateRequest,
    session_mgr: SessionManager = Depends(get_session_manager),
    thread_mgr: ThreadManager = Depends(get_thread_manager),
):
  """Create a thread and spawn a worker agent directly."""
  meta, cfg, resolved_backend, resolved_model = await _authorize_spawn_request(req, session_mgr)

  require_review = req.task_type == TaskType.IMPLEMENT

  # Create thread immediately so it's visible in the UI
  thread = await thread_mgr.create_thread(meta, req.description, context=req.context, require_review=require_review)

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
              task_type=req.task_type,
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
  _meta, cfg, resolved_backend, resolved_model = await _authorize_spawn_request(req, session_mgr)

  work_branch = req.work_branch or f"improve/{int(time.time())}"
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

  if req.watch_targets is not None:
    if len(req.watch_targets) == 0:
      raise HTTPException(status_code=400, detail="watch_targets must be non-empty when provided")
    if not all(t.pid > 0 for t in req.watch_targets):
      raise HTTPException(status_code=400, detail="watch_targets pids must be positive integers")
    has_local = any(t.host is None for t in req.watch_targets)
    has_remote = any(t.host is not None for t in req.watch_targets)
    if has_local and has_remote:
      raise HTTPException(
          status_code=400,
          detail="watch_targets must be all local or all remote, not mixed",
      )

  try:
    trigger = await trigger_mgr.create_trigger(
        req.session_id,
        req.delay_seconds,
        req.message,
        watch_targets=req.watch_targets,
    )
  except RemoteVerifyError as e:
    # Verify-on-create rejection: surface as 422 so the CLI exits with code 2.
    raise HTTPException(status_code=422, detail=str(e)) from e
  except RuntimeError as e:
    raise HTTPException(status_code=400, detail=str(e)) from e
  log.info(
      "trigger_scheduled",
      session=req.session_id,
      trigger_id=trigger.id,
      watch_targets=[t.model_dump() for t in (req.watch_targets or [])],
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
