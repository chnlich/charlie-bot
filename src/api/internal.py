"""Internal API endpoints — used by master CC to delegate tasks."""

import asyncio
import time

import structlog
from fastapi import APIRouter, Depends, HTTPException

from src.api.deps import (
  get_plan_manager,
  get_session_manager,
  get_thread_manager,
  get_trigger_manager,
)
from src.api.message_utils import build_agent_message_event
from src.core import event_types as ET
from src.core.config import CharlieBotConfig, get_config
from src.core.improve_command import (
  ImproveLoopAlreadyRunningError,
  loop_goal_path,
  loop_plan_path,
  reserve_loop_state,
  run_improve_loop,
)
from src.core.master_trigger import trigger_master
from src.core.models import (
  DelegateInvocationMetadata,
  DelegateRequest,
  ImproveRequest,
  PlanAmendRequest,
  PlanApproveRequest,
  PlanCloseRequest,
  PlanPresentRequest,
  ScheduleTriggerRequest,
  SessionMessageRequest,
  SessionMetadata,
  SessionStatus,
  SlackReplyRequest,
  SpawnRequest,
  TaskType,
  WatchKind,
)
from src.core.plans import PlanRegistryManager
from src.core.sessions import SessionManager
from src.core.slack_listener import SlackReplyError, post_reply
from src.core.spawner import (
  resolve_requested_subagent_backend_model,
  select_verify_backend,
  spawn_worker,
)
from src.core.takeoff_gate import DelegationBlockedError, check_takeoff_gate
from src.core.tasks import create_logged_task
from src.core.threads import ThreadManager
from src.core.triggers import RemoteVerifyError, TriggerManager

log = structlog.get_logger()

router = APIRouter()


@router.get("/version")
async def get_version():
  """Return the running server's build info (git SHA + UTC start time).

  Read-only; used by the CLI to detect version skew when an internal-API call fails
  (server older than the checkout → hint to restart).
  """
  from src.core.buildinfo import build_info
  return build_info()


def _delegate_invocation_event_payload(req: DelegateRequest) -> dict:
  """Return typed delegate invocation metadata for chat event persistence."""
  invocation = req.delegate_invocation
  if invocation is None:
    invocation = DelegateInvocationMetadata(
        task_type=req.task_type,
        repo_path=req.repo_path,
        base_branch=req.base_branch,
        task_spec_file=None,
        reviewer_context_file=None,
        keep_worktree=req.keep_worktree,
        backend=req.backend,
    )
  return invocation.model_dump(mode="json")


async def _authorize_spawn_request(
    req: DelegateRequest | ImproveRequest,
    session_mgr: SessionManager,
) -> tuple[SessionMetadata, CharlieBotConfig, str | None, str | None]:
  """Validate session, enforce takeoff gate, and resolve backend/model for spawn-style endpoints."""
  meta = await session_mgr.get_session(req.session_id)
  if not meta:
    raise HTTPException(status_code=404, detail="Session not found")

  if isinstance(req, DelegateRequest) and req.task_type == TaskType.VERIFY:
    pass
  else:
    try:
      await asyncio.to_thread(check_takeoff_gate, req.session_id, session_mgr)
    except DelegationBlockedError as e:
      raise HTTPException(status_code=403, detail=str(e)) from e

  cfg = get_config()
  try:
    if isinstance(req, DelegateRequest) and req.task_type == TaskType.VERIFY and req.backend is None:
      resolved_backend, resolved_model, _ = await select_verify_backend(req.session_id, cfg, session_mgr, [])
    else:
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
  if req.task_type == TaskType.VERIFY:
    if req.repo_path is not None:
      raise HTTPException(status_code=400, detail="verify delegations are repo-less; omit repo_path")
    if req.base_branch is not None:
      raise HTTPException(status_code=400, detail="verify delegations are repo-less; omit base_branch")
  else:
    if req.repo_path is None:
      raise HTTPException(status_code=400, detail=f"{req.task_type.value} delegations require repo_path")
    if req.base_branch is None:
      raise HTTPException(status_code=400, detail=f"{req.task_type.value} delegations require base_branch")

  meta, cfg, resolved_backend, resolved_model = await _authorize_spawn_request(req, session_mgr)

  require_review = req.task_type == TaskType.IMPLEMENT

  # Create thread immediately so it's visible in the UI
  thread = await thread_mgr.create_thread(
      meta,
      req.description,
      context=req.context,
      require_review=require_review,
      task_type=req.task_type,
  )

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
      "delegate_invocation": _delegate_invocation_event_payload(req),
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
        work_branch,
        req.repo_path,
        cfg,
        plan=req.plan,
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
          plan=req.plan,
      ),
      name=f"improve-loop-{req.session_id}",
  )

  log.info("improve_loop_started", session=req.session_id, iterations=req.iterations, goal=req.goal)

  response = {
      "status": "started",
      "session_id": req.session_id,
      "iterations": req.iterations,
      "loop_id": state.loop_id,
      "goal_path": str(loop_goal_path(req.session_id, state.loop_id, cfg)),
  }
  if req.plan is not None:
    response["plan_path"] = str(loop_plan_path(req.session_id, state.loop_id, cfg))
  return response


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
    for t in req.watch_targets:
      if t.kind == WatchKind.SLURM_JOB:
        if t.job_id <= 0:
          raise HTTPException(status_code=400, detail="watch_targets slurm job_id must be a positive integer")
      elif t.pid <= 0:
        raise HTTPException(status_code=400, detail="watch_targets pids must be positive integers")

  watch_probe: dict[str, str] = {}
  try:
    trigger = await trigger_mgr.create_trigger(
        req.session_id,
        req.delay_seconds,
        req.message,
        watch_targets=req.watch_targets,
        probe_out=watch_probe,
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

  response: dict = {"trigger_id": trigger.id, "fire_at": trigger.fire_at.isoformat()}
  if watch_probe:
    response["watch_probe"] = watch_probe
  return response


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


@router.post("/session-message")
async def session_message(
    req: SessionMessageRequest,
    session_mgr: SessionManager = Depends(get_session_manager),
    cfg: CharlieBotConfig = Depends(get_config),
):
  """Relay an agent message into another session's event log and wake its master.

  Persists an ``agent_message`` event (never a ``user`` event, so no takeoff
  window is minted or revoked), then wakes the target master with the relay
  prefix. The injected content bypasses slash-command dispatch; when the target
  session is mid-run the wake enqueues on the master work-item queue.
  """
  caller = await session_mgr.get_session(req.session_id)
  if caller is None:
    raise HTTPException(status_code=404, detail="Session not found")
  target = await session_mgr.get_session(req.target_session_id)
  if target is None:
    raise HTTPException(status_code=404, detail="Target session not found")
  if target.status == SessionStatus.ARCHIVED:
    raise HTTPException(status_code=409, detail="Target session is archived")

  await session_mgr.persist_and_broadcast(
      req.target_session_id,
      build_agent_message_event(
          req.content,
          from_session=caller.id,
          from_session_name=caller.name,
      ),
  )
  create_logged_task(
      trigger_master(
          req.target_session_id,
          f"[Message from session {caller.name}] {req.content}",
          cfg,
          session_mgr,
      ),
      name=f"session-message-relay-{req.target_session_id}",
  )
  log.info(
      "session_message_relayed",
      session=req.session_id,
      target_session=req.target_session_id,
      content_chars=len(req.content),
  )
  return {"status": "accepted"}


@router.post("/slack/reply")
async def slack_reply(
    req: SlackReplyRequest,
    session_mgr: SessionManager = Depends(get_session_manager),
    cfg: CharlieBotConfig = Depends(get_config),
):
  """Post the calling session's reply to its own Slack thread and return the readback.

  The in-process boundary behind ``charliebot slack reply``: the session's
  ``slack_origin`` names the thread, the running round's input names the summon
  the reply answers, and the readback (chars, chunks, over_budget, answers) is
  what the CLI prints. Refusals map SlackReplyError's status (404 unknown
  session, 409 no Slack thread, 422 blank text, 502 Slack rejected the post
  after retries); nothing is persisted on a refusal.
  """
  try:
    return await post_reply(req.session_id, req.text, cfg, session_mgr)
  except SlackReplyError as exc:
    raise HTTPException(status_code=exc.status, detail=exc.detail) from exc


# ---------------------------------------------------------------------------
# Plan registry verbs
# ---------------------------------------------------------------------------


def _build_base(req: PlanPresentRequest | PlanAmendRequest) -> dict | None:
  if req.base_repo is None and req.base_branch is None and req.base_sha is None:
    return None
  return {"repo": req.base_repo, "branch": req.base_branch, "sha": req.base_sha}


async def _authorize_plan_session(session_id: str, session_mgr: SessionManager) -> None:
  meta = await session_mgr.get_session(session_id)
  if not meta:
    raise HTTPException(status_code=404, detail="Session not found")


@router.post("/plan/present")
async def plan_present(
    req: PlanPresentRequest,
    session_mgr: SessionManager = Depends(get_session_manager),
    plan_mgr: PlanRegistryManager = Depends(get_plan_manager),
):
  """Register a new plan lineage (v1, trigger=initial)."""
  await _authorize_plan_session(req.session_id, session_mgr)
  try:
    return await plan_mgr.present(
        req.session_id,
        file=req.file,
        title=req.title,
        base=_build_base(req),
    )
  except ValueError as e:
    raise HTTPException(status_code=400, detail=str(e)) from e


@router.post("/plan/amend")
async def plan_amend(
    req: PlanAmendRequest,
    session_mgr: SessionManager = Depends(get_session_manager),
    plan_mgr: PlanRegistryManager = Depends(get_plan_manager),
):
  """Append the next version to a plan lineage."""
  await _authorize_plan_session(req.session_id, session_mgr)
  try:
    return await plan_mgr.amend(
        req.session_id,
        file=req.file,
        plan_id=req.plan_id,
        trigger=req.trigger,
        base=_build_base(req),
    )
  except ValueError as e:
    raise HTTPException(status_code=400, detail=str(e)) from e


@router.post("/plan/approve")
async def plan_approve(
    req: PlanApproveRequest,
    session_mgr: SessionManager = Depends(get_session_manager),
    plan_mgr: PlanRegistryManager = Depends(get_plan_manager),
):
  """Record a takeoff against the latest version of a plan lineage."""
  await _authorize_plan_session(req.session_id, session_mgr)
  try:
    return await plan_mgr.approve(req.session_id, plan_id=req.plan_id)
  except ValueError as e:
    raise HTTPException(status_code=400, detail=str(e)) from e


@router.post("/plan/close")
async def plan_close(
    req: PlanCloseRequest,
    session_mgr: SessionManager = Depends(get_session_manager),
    plan_mgr: PlanRegistryManager = Depends(get_plan_manager),
):
  """Terminate a plan lineage as superseded, abandoned, or completed."""
  await _authorize_plan_session(req.session_id, session_mgr)
  try:
    return await plan_mgr.close(req.session_id, req.plan_id, req.close_as)
  except ValueError as e:
    raise HTTPException(status_code=400, detail=str(e)) from e
