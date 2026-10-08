"""Internal API endpoints behind ``charliebot improve`` and ``charliebot improve-stop``."""

import time

from fastapi import APIRouter, Depends, HTTPException
from pydantic import BaseModel, ConfigDict

from src.features.improve.improve_command import (
    ImproveLoopAlreadyRunningError,
    ImproveState,
    loop_goal_path,
    loop_plan_path,
    reserve_loop_state,
    save_loop_state,
    stop_improve_loop,
)
from src.infra.config import CharlieBotConfig
from src.infra.log_once import LazyStructlogLogger
from src.infra.tasks import create_logged_task
from src.runtime import spawner_backends
from src.runtime.api.deps import bad_request, get_config_on_loop, get_session_store, get_task_manager, require_found
from src.runtime.session_store import SessionStore
from src.runtime.takeoff_gate import DelegationBlockedError
from src.runtime.task_sessions import TaskTreeManager

log = LazyStructlogLogger()

router = APIRouter()


class ImproveRequest(BaseModel):
  """Request body for the internal improve endpoint."""
  model_config = ConfigDict(extra="forbid")

  session_id: str
  repo_path: str
  base_branch: str
  backend: str | None = None
  iterations: int = 3
  goal: str
  plan: str | None = None
  work_branch: str | None = None
  merge_back: bool = False


class ImproveStopRequest(BaseModel):
  """Request body for the internal improve-stop endpoint."""
  model_config = ConfigDict(extra="forbid")

  session_id: str


@router.post("/improve/stop")
async def stop_improve(
    req: ImproveStopRequest,
    cfg: CharlieBotConfig = Depends(get_config_on_loop),
    store: SessionStore = Depends(get_session_store),
) -> dict:
  """Mark the session's running improve loop stopped (charliebot improve-stop).

  The loop ends after its current iteration; the next `charliebot improve` in
  the same session starts a new loop. No running loop is a 409, not an error
  to retry.
  """
  require_found(await store.get_session(req.session_id))
  if not await stop_improve_loop(req.session_id, cfg):
    raise HTTPException(status_code=409, detail="No active improve loop in this session")
  log.info("improve_loop_stopped", session=req.session_id)
  return {"status": "stopped", "session_id": req.session_id}


@router.post("/improve")
async def start_improve_loop(
    req: ImproveRequest,
    cfg: CharlieBotConfig = Depends(get_config_on_loop),
    store: SessionStore = Depends(get_session_store),
    task_mgr: TaskTreeManager = Depends(get_task_manager),
) -> dict:
  """Launch an iterative improvement loop on the task tree as a background task.

  One worker child task, one iteration Run per round (``sequence_ref``
  kind=improve), and one final sequence result delivered to the manager
  through the common report owner.
  """
  require_found(await store.get_session(req.session_id))
  return await _start_improve_sequence(req, cfg, task_mgr, store)


async def _start_improve_sequence(
    req: ImproveRequest,
    cfg: CharlieBotConfig,
    task_mgr: TaskTreeManager,
    store: SessionStore,
) -> dict:
  """The v2 improve path: one worker child, iteration Runs, one final report.

  The nearest-real-user-ancestor gate re-judges here (whatever credential
  carried the request), the loop state and the child are reserved under the
  stable ids, and the controller task owns the iterations from there.
  """
  from src.features.improve.improve_sequence import create_improve_child, run_improve_sequence

  try:
    await task_mgr.check_task_authorization(req.session_id)
  except DelegationBlockedError as e:
    raise HTTPException(status_code=403, detail=str(e)) from e

  # Backend/model resolution comes BEFORE the reservation (the legacy path's
  # order): an unknown requested backend must fail the request, never leak a
  # "running" loop state or the active lock in this live process.
  try:
    resolved_backend, resolved_model = await spawner_backends.resolve_requested_subagent_backend_model(
        req.session_id, cfg, store, requested_backend=req.backend)
  except ValueError as e:
    raise bad_request(e) from e

  work_branch = req.work_branch or f"improve/{int(time.time())}"
  try:
    state = await reserve_loop_state(
        req.session_id,
        req.goal,
        work_branch,
        req.repo_path,
        cfg,
        plan=req.plan,
        base_branch=req.base_branch,
        merge_back=req.merge_back,
        resolved_backend=resolved_backend,
        resolved_model=resolved_model or "",
    )
  except ImproveLoopAlreadyRunningError as e:
    raise HTTPException(status_code=409, detail=str(e)) from e

  try:
    child = await create_improve_child(
        task_mgr, req.session_id, state.loop_id, req.goal, repo_path=req.repo_path, base_branch=req.base_branch)
  except Exception as e:
    # The reservation is this live process's: a rejected child creation must
    # not leave a "running" loop stamped with the live pid — no controller is
    # ever spawned for it, and its active lock would block every later improve
    # request until the next restart's dirty-pid reconciliation.
    await _fail_reserved_loop(req.session_id, state, cfg)
    from src.runtime.api.sessions import _task_http_error
    raise _task_http_error(e) from e

  create_logged_task(
      run_improve_sequence(
          req.session_id,
          cfg,
          task_mgr,
          loop_id=state.loop_id,
          iterations=req.iterations,
          child_id=child.id,
          goal=req.goal),
      name=f"improve-sequence-{req.session_id[:8]}-{state.loop_id}",
  )
  log.info(
      "improve_sequence_started",
      session=req.session_id,
      loop_id=state.loop_id,
      child=child.id,
      iterations=req.iterations)
  response = {
      "status": "started",
      "session_id": req.session_id,
      "iterations": req.iterations,
      "loop_id": state.loop_id,
      "goal_path": str(loop_goal_path(req.session_id, state.loop_id, cfg)),
      "child_session_id": child.id,
  }
  if req.plan is not None:
    response["plan_path"] = str(loop_plan_path(req.session_id, state.loop_id, cfg))
  return response


async def _fail_reserved_loop(session_id: str, state: ImproveState, cfg: CharlieBotConfig) -> None:
  """Fail a freshly reserved improve loop whose admission failed after the reservation.

  Marks the state failed and clears the active lock, so the rejected request
  never leaves a permanently blocking active.lock or a "running" loop no
  controller owns.
  """
  from src.features.improve.improve_command import clear_active_loop_lock

  state.status = "failed"
  await save_loop_state(session_id, state, cfg)
  await clear_active_loop_lock(session_id, cfg)
