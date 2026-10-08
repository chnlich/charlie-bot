"""Internal API endpoints — used by master CC to delegate tasks."""

from typing import Protocol

from fastapi import APIRouter, Depends, HTTPException

from src.infra import event_types as ET
from src.infra.config import get_config
from src.infra.log_once import LazyStructlogLogger
from src.infra.models import (
    DelegateInvocationMetadata,
    DelegateRequest,
    ScheduleTriggerRequest,
    SessionMessageRequest,
    TaskType,
    WatchKind,
)
from src.runtime import spawner_backends
from src.runtime.api.deps import (
    bad_request,
    get_session_events,
    get_session_store,
    get_task_manager,
    get_trigger_manager,
    require_found,
)
from src.runtime.api.deps import require_caller as require_caller_dep
from src.runtime.session_events import SessionEvents
from src.runtime.session_store import SessionStore
from src.runtime.takeoff_gate import DelegationBlockedError, is_verify_exempt
from src.runtime.task_errors import TaskConflictError, TaskForbiddenError, TaskInvalidError, TaskNotFoundError
from src.runtime.task_sessions import TaskTreeManager
from src.runtime.triggers import ArchivedSessionError, PendingTriggerLimitError, RemoteVerifyError, TriggerManager

log = LazyStructlogLogger()

router = APIRouter()


@router.get("/version")
async def get_version() -> dict:
  """Return the running server's build info (git SHA + UTC start time).

  Read-only; used by the CLI to detect version skew when an internal-API call fails
  (server older than the checkout → hint to restart).
  """
  from src.infra.buildinfo import build_info
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


class _SpawnRequest(Protocol):
  """The fields every spawn-style request body carries for ``_authorize_spawn_request``."""
  session_id: str
  backend: str | None


async def _authorize_spawn_request(
    req: _SpawnRequest,
    store: SessionStore,
    task_mgr: TaskTreeManager,
) -> tuple[str | None, str | None]:
  """Validate session, enforce the takeoff gate, and resolve backend/model for spawn-style endpoints.

  A task-tree node takes the one central authorization owner —
  ``TaskTreeManager.check_task_authorization``, the nearest-real-user-ancestor
  gate (judged where the delegation request enters; the Run launch re-judges
  nothing).
  A node inherits authorization from its nearest real-user ancestor.
  """
  meta = require_found(await store.get_session(req.session_id))

  if not (isinstance(req, DelegateRequest) and is_verify_exempt(req.task_type)):
    try:
      await task_mgr.check_task_authorization(req.session_id)
    except DelegationBlockedError as e:
      raise HTTPException(status_code=403, detail=str(e)) from e

  cfg = get_config()
  try:
    if isinstance(req, DelegateRequest) and req.task_type == TaskType.VERIFY and req.backend is None:
      resolved_backend, resolved_model, _ = await spawner_backends.select_verify_backend(req.session_id, cfg, store, [])
    else:
      resolved_backend, resolved_model = await spawner_backends.resolve_requested_subagent_backend_model(
          req.session_id, cfg, store, requested_backend=req.backend)
  except ValueError as e:
    raise bad_request(e) from e

  return resolved_backend, resolved_model


def delegate_request_id(req: DelegateRequest) -> str:
  """The delegation's stable operation id.

  An explicit ``request_id`` names intentional same-spec siblings; the derived
  default binds one (session, task type, spec body) to one operation, so a
  replayed CLI call after a lost response returns the original child instead
  of a second process.
  """
  if req.request_id:
    return req.request_id
  from src.runtime.control_events import derived_delegate_request_id
  return derived_delegate_request_id(req.session_id, req.task_type.value, req.description)


async def _delegate_task_tree(
    req: DelegateRequest,
    task_mgr: TaskTreeManager,
    session_events: SessionEvents,
    caller: object,
    resolved_backend: str,
    resolved_model: str | None,
) -> dict:
  """The v2 delegation: one worker child task with its first work Run.

  The child is a task-tree node (profile=worker) under the calling manager
  task; the Run is its first execution record. A replayed request returns the
  original child and Run. The returned ``thread_id`` equals ``run_id``.
  """
  from src.infra.models import RunRecord, TaskSpec
  from src.runtime.control_events import stable_run_id
  from src.runtime.task_sessions import canonical_task_spec_text

  try:
    # The nearest-user-ancestor gate judges here, where the delegation request
    # enters; the child run's launch re-judges nothing. The read-only verify
    # exemption rides the one shared judgment (is_verify_exempt) here, at the
    # admission check above, and at the agent-creation check; the structural
    # create checks still apply to a verify child.
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
  except (DelegationBlockedError, TaskNotFoundError, TaskForbiddenError, TaskConflictError, TaskInvalidError) as e:
    from src.runtime.api.sessions import _task_http_error
    raise _task_http_error(e) from e

  # The same adapter the creating tree owns launches the run — never a
  # differently-configured singleton.
  adapter = task_mgr.dispatch.executor
  from src.runtime.task_execution import TaskExecutionAdapter
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
  await session_events.persist_and_broadcast(req.session_id, task_event)

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
    session_events: SessionEvents = Depends(get_session_events),
    store: SessionStore = Depends(get_session_store),
    task_mgr: TaskTreeManager = Depends(get_task_manager),
    caller: object = Depends(require_caller_dep),
) -> dict:
  """Create a worker task under the calling manager and launch its first Run.

  The existing task node supplies the authorization boundary and the new
  worker child is created directly beneath it.
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
    # implement/quick-edit/script-run carry a repo, or neither field: a
    # repo-less Run works from its Run directory, and a base without its repo
    # (or the reverse) names a worktree that cannot exist.
    if (req.repo_path is None) != (req.base_branch is None):
      raise HTTPException(
          status_code=400,
          detail=f"{req.task_type.value} delegations take repo_path and base_branch together; "
          "give both for a repo task, neither for a repo-less one")
  require_found(await store.get_session(req.session_id))
  resolved_backend, resolved_model = await _authorize_spawn_request(req, store, task_mgr)
  return await _delegate_task_tree(req, task_mgr, session_events, caller, resolved_backend, resolved_model)


@router.post("/schedule-trigger")
async def schedule_trigger(
    req: ScheduleTriggerRequest,
    store: SessionStore = Depends(get_session_store),
    trigger_mgr: TriggerManager = Depends(get_trigger_manager),
) -> dict:
  """Schedule a delayed trigger that will wake the master CC after a delay."""
  require_found(await store.get_session(req.session_id))

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
  except (RemoteVerifyError, ArchivedSessionError, PendingTriggerLimitError) as e:
    # Verify-on-create rejection, a target archived without a successor, or a
    # registration past the session's pending-trigger limit: surface as 422 so
    # the CLI exits with code 2.
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
    store: SessionStore = Depends(get_session_store),
    task_mgr: TaskTreeManager = Depends(get_task_manager),
) -> dict:
  """Relay an agent message into another session's event log and wake its master.

  Persists an ``agent_message`` event (never a ``user`` event, so no takeoff
  window is minted or revoked), then wakes the target master with the relay
  prefix. A mid-run target session enqueues the wake on the master
  work-item queue. An archived task target refuses the relay with 409
  (``task <id> is archived``);
  only the user's own message restores an archived node.
  """
  caller = require_found(await store.get_session(req.session_id))
  target = await store.get_session(req.target_session_id)
  if target is None:
    raise HTTPException(status_code=404, detail="Target session not found")

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
  except (TaskNotFoundError, TaskForbiddenError, TaskConflictError, TaskInvalidError) as e:
    from src.runtime.api.sessions import _task_http_error
    raise _task_http_error(e) from e
  log.info(
      "session_message_dispatched",
      session=req.session_id,
      target_session=req.target_session_id,
      content_chars=len(req.content),
  )
  return {"status": "accepted"}
