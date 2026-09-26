"""Internal API endpoints — used by master CC to delegate tasks."""

import asyncio
import time

from fastapi import APIRouter, Depends, HTTPException

from src.api.deps import (
    bad_request,
    get_config_on_loop,
    get_plan_manager,
    get_session_manager,
    get_task_manager,
    get_trigger_manager,
    require_found,
)
from src.api.deps import (
    require_caller as require_caller_dep,)
from src.api.message_utils import build_agent_message_event
from src.core import event_types as ET
from src.core.config import CharlieBotConfig, get_config
from src.core.improve_command import (
    ImproveLoopAlreadyRunningError,
    ImproveState,
    loop_goal_path,
    loop_plan_path,
    reserve_loop_state,
    save_loop_state,
)
from src.core.log_once import LazyStructlogLogger
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
    SlackAckRequest,
    SlackReplyRequest,
    TaskType,
    WatchKind,
)
from src.core.plans import PlanRegistryManager
from src.core.sessions import SessionManager
from src.core.slack_listener import (
    SlackReplyError,
    ack_messages,
    assert_thread_fresh,
    post_reply,
)
from src.core.spawner import (
    resolve_requested_subagent_backend_model,
    select_verify_backend,
)
from src.core.takeoff_gate import DelegationBlockedError, check_takeoff_gate, is_verify_exempt
from src.core.task_sessions import TaskTreeManager
from src.core.tasks import create_logged_task
from src.core.triggers import ArchivedSessionError, RemoteVerifyError, TriggerManager

log = LazyStructlogLogger()

router = APIRouter()


@router.get("/version")
async def get_version() -> dict:
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
    task_mgr: TaskTreeManager,
) -> tuple[SessionMetadata, CharlieBotConfig, str | None, str | None]:
  """Validate session, enforce the takeoff gate, and resolve backend/model for spawn-style endpoints.

  A v2 task-tree node takes the one central v2 authorization owner —
  ``TaskTreeManager.check_task_authorization``, the nearest-real-user-ancestor
  gate (re-judged by the v2 helper below and again at the actual launch).
  The session-local legacy gate must not pre-gate a node that legitimately
  inherits an ancestor's authorization: that split would force every sub-task
  manager to carry its own take-off before the real CLI route works. The
  read-only verify exemption is preserved for both v1 and v2.
  """
  meta = require_found(await session_mgr.get_session(req.session_id))

  if isinstance(req, DelegateRequest) and is_verify_exempt(req.task_type):
    pass  # the read-only verify exemption (v1 and v2 alike)
  elif meta.profile is not None:
    try:
      await task_mgr.check_task_authorization(req.session_id)
    except DelegationBlockedError as e:
      raise HTTPException(status_code=403, detail=str(e)) from e
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
    raise bad_request(e) from e

  return meta, cfg, resolved_backend, resolved_model


def delegate_request_id(req: DelegateRequest) -> str:
  """The delegation's stable operation id.

  An explicit ``request_id`` names intentional same-spec siblings; the derived
  default binds one (session, task type, spec body) to one operation, so a
  replayed CLI call after a lost response returns the original child instead
  of a second process.
  """
  if req.request_id:
    return req.request_id
  from src.core.control_events import sha256_hex
  return "delegate-" + sha256_hex("\x00".join([req.session_id, str(req.task_type.value), req.description]))[:24]


async def _delegate_task_tree(
    req: DelegateRequest,
    meta: SessionMetadata,
    cfg: CharlieBotConfig,
    task_mgr: TaskTreeManager,
    session_mgr: SessionManager,
    caller: object,
    resolved_backend: str,
    resolved_model: str | None,
) -> dict:
  """The v2 delegation: one worker child task with its first work Run.

  The child is a task-tree node (profile=worker) under the calling manager
  task; the Run is its first execution record. A replayed request returns the
  original child and Run. The returned ``thread_id`` is the compatibility
  alias the legacy thread routes resolve to the same Run.
  """
  from src.core.control_events import stable_run_id
  from src.core.models import RunRecord, TaskSpec
  from src.core.task_sessions import (
      TaskConflictError,
      TaskForbiddenError,
      TaskInvalidError,
      TaskNotFoundError,
      canonical_task_spec_text,
  )

  try:
    # The nearest-user-ancestor gate re-judges at delegation and again at the
    # child run's actual launch, whatever credential carries the request. The
    # read-only verify exemption rides the one shared judgment (is_verify_exempt)
    # here, at the admission check above, at the agent-creation check, and at
    # the launch; the structural create checks still apply to a verify child.
    if not is_verify_exempt(req.task_type):
      await task_mgr.check_task_authorization(req.session_id)
    request_id = delegate_request_id(req)
    task_spec = TaskSpec(
        goal=req.description,
        context_refs=[req.context] if req.context else [],
        repo_path=req.repo_path,
        base_branch=req.base_branch,
        task_type=req.task_type,
        keep_worktree=req.keep_worktree,
    )
    child = await task_mgr.create_task(
        request_id=request_id,
        task_parent_id=req.session_id,
        profile="worker",
        task=task_spec,
        name=None,
        backend=None,
        caller=caller,
    )
    run_id = stable_run_id(child.id, f"{request_id}:work")
    existing = await task_mgr.runs.get_run(child.id, run_id)
    if existing is None:
      if task_mgr.task_state(child.id) != "open":
        log.info("delegate_child_closed", parent=req.session_id, child=child.id)
        return {
            "session_id": child.id,
            "parent_session_id": req.session_id,
            "run_id": None,
            "thread_id": None,
            "description": req.description,
        }
      record = RunRecord(
          id=run_id,
          session_id=child.id,
          kind="work",
          backend=resolved_backend,
          model=resolved_model,
          repo_path=req.repo_path,
          base_branch=req.base_branch,
      )
      async with task_mgr.control_lock:
        await task_mgr.runs.register_run_locked(record, task_spec_text=canonical_task_spec_text(task_spec))
        # The parent-entry compatibility alias: legacy thread routes addressed
        # from the delegating session resolve to the same Run (whose owner is
        # the child task).
        task_mgr.aliases.register_owner_thread_alias(req.session_id, child.id, run_id)
  except DelegationBlockedError as e:
    # The central ancestor gate's refusal (worker caller, closed ancestor,
    # shadowing local instruction, expired window) is an authorization
    # rejection, never a server error.
    raise HTTPException(status_code=403, detail=str(e)) from e
  except TaskNotFoundError as e:
    raise HTTPException(status_code=404, detail=str(e)) from e
  except TaskForbiddenError as e:
    raise HTTPException(status_code=403, detail=str(e)) from e
  except TaskConflictError as e:
    blockers = list(getattr(e, "blockers", None) or [])
    raise HTTPException(status_code=409, detail={"message": str(e), "blockers": blockers}) from e
  except TaskInvalidError as e:
    raise HTTPException(status_code=400, detail=str(e)) from e

  # The same adapter the creating tree owns launches the run — never a
  # differently-configured singleton.
  adapter = task_mgr.dispatch.executor
  from src.core.task_execution import TaskExecutionAdapter
  if not isinstance(adapter, TaskExecutionAdapter):
    raise HTTPException(status_code=503, detail="task execution adapter is not installed")
  adapter.launch(child.id, run_id)

  # Save and broadcast task_delegated so the cursor stays in sync on reconnect.
  task_event = {
      "type": ET.TASK_DELEGATED,
      "thread_id": run_id,
      "description": req.description,
      "backend": resolved_backend or "",
      "model": resolved_model or "",
      "child_session_id": child.id,
      ET.DELEGATE_INVOCATION: _delegate_invocation_event_payload(req),
  }
  # The delegation card rides the delegating session's chat (persist + broadcast).
  await session_mgr.persist_and_broadcast(req.session_id, task_event)

  log.info("task_delegated_task_tree", session=req.session_id, child=child.id, run_id=run_id)
  return {
      "session_id": child.id,
      "parent_session_id": req.session_id,
      "run_id": run_id,
      "thread_id": run_id,
      "description": req.description,
  }


@router.post("/delegate")
async def delegate_task(
    req: DelegateRequest,
    session_mgr: SessionManager = Depends(get_session_manager),
    task_mgr: TaskTreeManager = Depends(get_task_manager),
    caller: object = Depends(require_caller_dep),
) -> dict:
  """Create a worker task under the calling manager and launch its first Run.

  A legacy session (profile None) delegates in place: its own session-local
  gate and backend check run first so a blocked or malformed request converts
  nothing, and the worker child is created under it without rewriting it.
  """
  # Repo/branch contract first, before any session access or backend
  # resolution: the rejection must not depend on the caller's configured
  # backends, and a replayed request must fail identically.
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
  require_found(await session_mgr.get_session(req.session_id))
  meta, cfg, resolved_backend, resolved_model = await _authorize_spawn_request(req, session_mgr, task_mgr)
  return await _delegate_task_tree(req, meta, cfg, task_mgr, session_mgr, caller, resolved_backend, resolved_model)


@router.post("/improve")
async def start_improve_loop(
    req: ImproveRequest,
    cfg: CharlieBotConfig = Depends(get_config_on_loop),
    session_mgr: SessionManager = Depends(get_session_manager),
    task_mgr: TaskTreeManager = Depends(get_task_manager),
) -> dict:
  """Launch an iterative improvement loop on the task tree as a background task.

  One worker child task, one iteration Run per round (``sequence_ref``
  kind=improve), and one final sequence result delivered to the manager
  through the common report owner. A legacy session (profile None) loops in
  place: its own session-local gate and backend check run first so a blocked
  or malformed request converts nothing, and the worker child is created
  under it without rewriting it.
  """
  target = require_found(await session_mgr.get_session(req.session_id))
  if target.profile is None:
    await _authorize_spawn_request(req, session_mgr, task_mgr)
  return await _start_improve_sequence(req, cfg, task_mgr, session_mgr)


async def _start_improve_sequence(
    req: ImproveRequest,
    cfg: CharlieBotConfig,
    task_mgr: TaskTreeManager,
    session_mgr: SessionManager,
) -> dict:
  """The v2 improve path: one worker child, iteration Runs, one final report.

  The nearest-real-user-ancestor gate re-judges here (whatever credential
  carried the request), the loop state and the child are reserved under the
  stable ids, and the controller task owns the iterations from there.
  """
  from src.core.improve_sequence import create_improve_child, run_improve_sequence
  from src.core.task_sessions import (
      TaskConflictError,
      TaskForbiddenError,
      TaskInvalidError,
      TaskNotFoundError,
  )

  try:
    await task_mgr.check_task_authorization(req.session_id)
  except DelegationBlockedError as e:
    raise HTTPException(status_code=403, detail=str(e)) from e

  # Backend/model resolution comes BEFORE the reservation (the legacy path's
  # order): an unknown requested backend must fail the request, never leak a
  # "running" loop state or the active lock in this live process.
  try:
    resolved_backend, resolved_model = await resolve_requested_subagent_backend_model(
        req.session_id, cfg, session_mgr, requested_backend=req.backend)
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
    if isinstance(e, TaskNotFoundError):
      raise HTTPException(status_code=404, detail=str(e)) from e
    if isinstance(e, TaskForbiddenError):
      raise HTTPException(status_code=403, detail=str(e)) from e
    if isinstance(e, TaskConflictError):
      blockers = list(getattr(e, "blockers", None) or [])
      raise HTTPException(status_code=409, detail={"message": str(e), "blockers": blockers}) from e
    if isinstance(e, TaskInvalidError):
      raise HTTPException(status_code=400, detail=str(e)) from e
    raise

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
  from src.core.improve_command import clear_active_loop_lock

  state.status = "failed"
  await save_loop_state(session_id, state, cfg)
  await clear_active_loop_lock(session_id, cfg)


@router.post("/schedule-trigger")
async def schedule_trigger(
    req: ScheduleTriggerRequest,
    session_mgr: SessionManager = Depends(get_session_manager),
    trigger_mgr: TriggerManager = Depends(get_trigger_manager),
) -> dict:
  """Schedule a delayed trigger that will wake the master CC after a delay."""
  require_found(await session_mgr.get_session(req.session_id))

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
  except (RemoteVerifyError, ArchivedSessionError) as e:
    # Verify-on-create rejection, or a target archived without a successor:
    # surface as 422 so the CLI exits with code 2.
    raise HTTPException(status_code=422, detail=str(e)) from e
  except RuntimeError as e:
    raise bad_request(e) from e
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
) -> dict:
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
    cfg: CharlieBotConfig = Depends(get_config_on_loop),
    task_mgr: TaskTreeManager = Depends(get_task_manager),
) -> dict:
  """Relay an agent message into another session's event log and wake its master.

  Persists an ``agent_message`` event (never a ``user`` event, so no takeoff
  window is minted or revoked), then wakes the target master with the relay
  prefix. The injected content bypasses slash-command dispatch; when the target
  session is mid-run the wake enqueues on the master work-item queue. An
  archived target still receives the event: the wake that follows pulls it
  back to active.
  """
  caller = require_found(await session_mgr.get_session(req.session_id))
  target = await session_mgr.get_session(req.target_session_id)
  if target is None:
    raise HTTPException(status_code=404, detail="Target session not found")

  # A v2 target takes the durable dispatcher path: the relay keeps its
  # agent_message identity and the caller's provenance (never a real user
  # event), persists durably first, and the executor seam decides any launch.
  if target.profile is not None and task_mgr is not None:
    from src.core.task_sessions import (
        TaskConflictError,
        TaskForbiddenError,
        TaskInvalidError,
        TaskNotFoundError,
    )

    try:
      await task_mgr.dispatch.admit_input(
          req.target_session_id,
          event_type=ET.AGENT_MESSAGE,
          content=req.content,
          actor="agent",
          from_session=caller.id,
          from_session_name=caller.name,
      )
      await task_mgr.dispatch.dispatch_pending(req.target_session_id)
    except TaskNotFoundError as e:
      raise HTTPException(status_code=404, detail=str(e)) from e
    except TaskForbiddenError as e:
      raise HTTPException(status_code=403, detail=str(e)) from e
    except TaskConflictError as e:
      blockers = list(getattr(e, "blockers", None) or [])
      raise HTTPException(status_code=409, detail={"message": str(e), "blockers": blockers}) from e
    except TaskInvalidError as e:
      raise HTTPException(status_code=400, detail=str(e)) from e
    log.info(
        "session_message_dispatched",
        session=req.session_id,
        target_session=req.target_session_id,
        content_chars=len(req.content),
    )
    return {"status": "accepted"}

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
    cfg: CharlieBotConfig = Depends(get_config_on_loop),
) -> dict:
  """Post the calling session's reply to its own Slack thread and return the readback.

  The in-process boundary behind ``charliebot slack reply``: the session's
  ``slack_origin`` names the thread, the running round's input names the summon
  the reply answers, and the readback (chars, chunks, over_budget, answers) is
  what the CLI prints. Refusals map SlackReplyError's status (404 unknown
  session, 409 no Slack thread, 422 blank text, 502 Slack rejected the post
  after retries); nothing is persisted on a refusal. Freshness is gated first:
  eligible thread messages above the session's watermark refuse with a 412
  ``stale_thread`` payload naming each unseen message, before any chunk posts.
  """
  try:
    await assert_thread_fresh(req.session_id, cfg, session_mgr)
    return await post_reply(req.session_id, req.text, cfg, session_mgr)
  except SlackReplyError as exc:
    raise HTTPException(status_code=exc.status, detail=exc.detail) from exc


@router.post("/slack/ack")
async def slack_ack(
    req: SlackAckRequest,
    session_mgr: SessionManager = Depends(get_session_manager),
    cfg: CharlieBotConfig = Depends(get_config_on_loop),
) -> dict:
  """Mark the calling session's read thread messages as consumed and return the readback.

  The boundary behind ``charliebot slack ack``: ``message_ids`` are Slack ts
  values, every one must be eligible, and every eligible id at or below the
  newest must be included — a skipped id (or an unknown/ineligible one) refuses
  with 422 naming it and persists nothing. Success advances the session's
  ``slack_watermark_ts``, persists a small ack event for the audit trail, and
  returns ``acked`` plus the new watermark; re-acking ids at or below the
  watermark is an idempotent no-op counted as acked. Refusals map
  SlackReplyError's status: 404 unknown session, 409 no Slack thread.
  """
  try:
    return await ack_messages(req.session_id, req.message_ids, cfg, session_mgr)
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
  require_found(await session_mgr.get_session(session_id))


@router.post("/plan/present")
async def plan_present(
    req: PlanPresentRequest,
    session_mgr: SessionManager = Depends(get_session_manager),
    plan_mgr: PlanRegistryManager = Depends(get_plan_manager),
) -> dict:
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
    raise bad_request(e) from e


@router.post("/plan/amend")
async def plan_amend(
    req: PlanAmendRequest,
    session_mgr: SessionManager = Depends(get_session_manager),
    plan_mgr: PlanRegistryManager = Depends(get_plan_manager),
) -> dict:
  """Append the next version to a plan lineage."""
  await _authorize_plan_session(req.session_id, session_mgr)
  try:
    return await plan_mgr.amend(
        req.session_id,
        file=req.file,
        plan_id=req.plan_id,
        trigger=req.trigger,
        base=_build_base(req),
        note=req.note,
    )
  except ValueError as e:
    raise bad_request(e) from e


@router.post("/plan/approve")
async def plan_approve(
    req: PlanApproveRequest,
    session_mgr: SessionManager = Depends(get_session_manager),
    plan_mgr: PlanRegistryManager = Depends(get_plan_manager),
) -> dict:
  """Record a takeoff against the latest version of a plan lineage."""
  await _authorize_plan_session(req.session_id, session_mgr)
  try:
    return await plan_mgr.approve(req.session_id, plan_id=req.plan_id)
  except ValueError as e:
    raise bad_request(e) from e


@router.post("/plan/close")
async def plan_close(
    req: PlanCloseRequest,
    session_mgr: SessionManager = Depends(get_session_manager),
    plan_mgr: PlanRegistryManager = Depends(get_plan_manager),
) -> dict:
  """Terminate a plan lineage as superseded, abandoned, or completed."""
  await _authorize_plan_session(req.session_id, session_mgr)
  try:
    return await plan_mgr.close(req.session_id, req.plan_id, req.close_as)
  except ValueError as e:
    raise bad_request(e) from e
