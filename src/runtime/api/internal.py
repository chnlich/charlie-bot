"""Internal API endpoints — used by master CC to delegate tasks."""

import pathlib

import fastapi

from src.infra import buildinfo, config, constants, log_once, models, text_length
from src.infra import event_types as ET
from src.runtime import (
    backend_pool,
    control_events,
    run_token,
    session_events,
    session_store,
    spawner_backends,
    takeoff_gate,
    task_errors,
    task_execution,
    task_sessions,
    triggers,
)
from src.runtime.api import deps, sessions

log = log_once.LazyStructlogLogger()

router = fastapi.APIRouter()


@router.get("/version")
async def get_version() -> dict:
  """Return the running server's build info (git SHA + UTC start time).

  Read-only; used by the CLI to detect version skew when an internal-API call fails
  (server older than the checkout → hint to restart).
  """
  return buildinfo.build_info()


def _delegate_invocation_event_payload(req: models.DelegateRequest) -> dict:
  """Return typed delegate invocation metadata for chat event persistence."""
  invocation = req.delegate_invocation
  if invocation is None:
    invocation = models.DelegateInvocationMetadata(
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
    req: models.DelegateRequest,
    store: session_store.SessionStore,
    task_mgr: task_sessions.TaskTreeManager,
) -> tuple[str | None, str | None, bool]:
  """Validate session, enforce the runtime gate, and resolve backend/model for a delegation.

  The third value is True when the backends.pool chose the backend (the delegation named none).
  The fourth is the router's route object when the router ordered the backend (an unpinned
  delegation of a routed role with ``backends.routing`` configured); None otherwise.

  A task-tree node takes the one central authorization owner —
  ``task_sessions.TaskTreeManager.check_task_authorization``, the runtime gate (judged where
  the delegation request enters; the Run launch re-judges nothing): an open
  manager chain with a real user message or a thread platform summon on some
  node of it.
  """
  deps.require_found(await store.get_session(req.session_id))

  if not takeoff_gate.is_read_only_exempt(req.task_type):
    try:
      await task_mgr.check_task_authorization(req.session_id)
    except takeoff_gate.DelegationBlockedError as e:
      raise fastapi.HTTPException(status_code=403, detail=str(e)) from e

  cfg = config.get_config()
  route = None
  if req.backend is None and req.task_type in _ROUTED_ROLES and cfg.backends.routing is not None:
    route = await _route_unpinned_task(cfg, req, store)
    option = backend_pool.pick_ordered(cfg, caller=req.task_type.value, backends=route.backends)
    if option is not None:
      return option.id, models.option_default_model(option, subject="routed backend "), False, route
    # Every routed member is cooling (or the router returned the empty list): the pool decides,
    # with the route still recorded on the node.
  try:
    if req.task_type in (models.TaskType.VERIFY, models.TaskType.COLD_READ) and req.backend is None:
      resolved_backend, resolved_model, _ = await spawner_backends.select_verify_backend(
          req.session_id, cfg, store, [], caller=req.task_type.value)
      backend_from_pool = resolved_backend in cfg.backends.pool
    else:
      resolved_backend, resolved_model, backend_from_pool = (
          await spawner_backends.resolve_requested_subagent_backend_model(
              req.session_id, cfg, store, requested_backend=req.backend, role=req.task_type))
  except ValueError as e:
    raise deps.bad_request(e) from e

  return resolved_backend, resolved_model, backend_from_pool, route


# The delegation entry's unpinned roles that ask the router first. Every other delegation — cold
# read, session naming, the page check, the improve loop and pinned work — resolves its backend
# directly.
_ROUTED_ROLES = frozenset(
    {models.TaskType.IMPLEMENT, models.TaskType.QUICK_EDIT, models.TaskType.SCRIPT_RUN, models.TaskType.VERIFY})


async def _route_unpinned_task(
    cfg: config.CharlieBotConfig,
    req: models.DelegateRequest,
    store: session_store.SessionStore,
) -> backend_pool.BackendRoute:
  """Ask the router to order the candidate backends for one unpinned delegation.

  Verify checks the session's own work, so the session's backend leaves the candidates; the worker
  roles avoid nothing. A router failure surfaces as a 502 whose body carries the router error — it
  never falls back to the pool silently.
  """
  session_meta = await store.get_session(req.session_id)
  avoided = set()
  if req.task_type == models.TaskType.VERIFY:
    avoided.add(spawner_backends.session_backend_id(cfg, session_meta))
  candidates = [option.id for option in cfg.backends.options if option.id not in avoided]
  try:
    return await backend_pool.route(
        cfg,
        role=req.task_type.value,
        text=f"Task type: {req.task_type.value}\n\n{req.description}",
        candidates=candidates)
  except backend_pool.RouterError as e:
    raise fastapi.HTTPException(status_code=502, detail=f"backend router failed: {e}") from e


def delegate_request_id(req: models.DelegateRequest) -> str:
  """The delegation's stable operation id.

  An explicit ``request_id`` names intentional same-spec siblings; the derived
  default binds one (session, task type, spec body) to one operation, so a
  replayed CLI call after a lost response returns the original child instead
  of a second process.
  """
  if req.request_id:
    return req.request_id
  return control_events.derived_delegate_request_id(req.session_id, req.task_type.value, req.description)


async def _delegate_task_tree(
    req: models.DelegateRequest,
    task_mgr: task_sessions.TaskTreeManager,
    events: session_events.SessionEvents,
    caller: object,
    resolved_backend: str,
    resolved_model: str | None,
    backend_from_pool: bool,
    route: backend_pool.BackendRoute | None,
) -> dict:
  """The v2 delegation: one worker child task with its first work Run.

  The child is a task-tree node (profile=worker) under the calling manager
  task; the Run is its first execution record. A replayed request returns the
  original child and Run. The returned ``thread_id`` equals ``run_id``.
  """

  try:
    # The runtime gate judges here, where the delegation request enters; the
    # child run's launch re-judges nothing. The read-only exemption rides the
    # one shared judgment (takeoff_gate.is_read_only_exempt) here, at the admission check
    # above, and at the agent-creation check; the structural create checks
    # still apply to a read-only child.
    if not takeoff_gate.is_read_only_exempt(req.task_type):
      await task_mgr.check_task_authorization(req.session_id)
    request_id = delegate_request_id(req)
    task_spec = models.TaskSpec(
        goal=req.description,
        repo_path=req.repo_path,
        base_branch=req.base_branch,
        task_type=req.task_type,
        keep_worktree=req.keep_worktree,
        cold_read=req.cold_read,
    )
    # A cold-read child is named at creation (no automatic naming call); every
    # other child takes the default node name.
    name = (
        f"Cold read: {pathlib.Path(req.cold_read.page_path).name}"
        if req.task_type == models.TaskType.COLD_READ else None)
    child = await task_mgr.create_task(
        request_id=request_id,
        task_parent_id=req.session_id,
        profile="worker",
        task=task_spec,
        name=name,
        # The dispatcher reserves every later work Run of this node from the
        # child record's backend, so the record carries the delegation's
        # resolved backend.
        backend=resolved_backend,
        caller=caller,
    )
    if backend_from_pool or route is not None:
      # Before the first launch: a quota failure on this node fails over only when the node says so.
      slot_fields = {"backend_from_pool": backend_from_pool}
      if route is not None:
        slot_fields["route"] = route.model_dump(mode="json")
      await task_mgr.update_slot_fields(child.id, backend_pool.METADATA_OWNER, **slot_fields)
    run_id = control_events.stable_run_id(child.id, f"{request_id}:work")
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
      record = models.RunRecord(
          id=run_id,
          session_id=child.id,
          kind="work",
          backend=resolved_backend,
          model=resolved_model,
          repo_path=req.repo_path,
          base_branch=req.base_branch,
      )
      async with task_mgr.control_lock:
        await task_mgr.runs.register_run_locked(
            record, task_spec_text=task_sessions.canonical_task_spec_text(task_spec))
  except (takeoff_gate.DelegationBlockedError, task_errors.TaskNotFoundError, task_errors.TaskForbiddenError,
          task_errors.TaskConflictError, task_errors.TaskInvalidError) as e:
    raise sessions._task_http_error(e) from e

  # The same adapter the creating tree owns launches the run — never a
  # differently-configured singleton.
  adapter = task_mgr.dispatch.executor
  if not isinstance(adapter, task_execution.TaskExecutionAdapter):
    raise fastapi.HTTPException(status_code=503, detail="task execution adapter is not installed")
  adapter.launch(child.id, run_id)

  # Save and broadcast task_delegated so the cursor stays in sync on reconnect.
  task_event = {
      "type": ET.TASK_DELEGATED,
      "thread_id": run_id,
      "description": req.description,
      "backend": resolved_backend or "",
      "model": resolved_model or "",
      "child_session_id": child.id,
      "route": route.model_dump(mode="json") if route is not None else None,
      ET.DELEGATE_INVOCATION: _delegate_invocation_event_payload(req),
  }
  # The delegation card rides the delegating session's chat (persist + broadcast).
  await events.persist_and_broadcast(req.session_id, task_event)

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
    req: models.DelegateRequest,
    events: session_events.SessionEvents = fastapi.Depends(deps.get_session_events),
    store: session_store.SessionStore = fastapi.Depends(deps.get_session_store),
    task_mgr: task_sessions.TaskTreeManager = fastapi.Depends(deps.get_task_manager),
    caller: object = fastapi.Depends(deps.require_caller),
) -> dict:
  """Create a worker task under the calling manager and launch its first Run.

  The existing task node supplies the authorization boundary and the new
  worker child is created directly beneath it.
  """
  # Field contract first, before any session access or backend resolution:
  # the rejection must not depend on the caller's configured backends, and a
  # replayed request must fail identically. The cold_read field rides exactly
  # the cold-read task type.
  if req.task_type == models.TaskType.COLD_READ and req.cold_read is None:
    raise fastapi.HTTPException(status_code=400, detail="cold-read delegations require cold_read")
  if req.task_type != models.TaskType.COLD_READ and req.cold_read is not None:
    raise fastapi.HTTPException(status_code=400, detail=f"{req.task_type.value} delegations reject cold_read")
  if req.task_type in (models.TaskType.VERIFY, models.TaskType.COLD_READ):
    if req.repo_path is not None:
      raise fastapi.HTTPException(
          status_code=400, detail=f"{req.task_type.value} delegations are repo-less; omit repo_path")
    if req.base_branch is not None:
      raise fastapi.HTTPException(
          status_code=400, detail=f"{req.task_type.value} delegations are repo-less; omit base_branch")
  else:
    # implement/quick-edit/script-run carry a repo, or neither field: a
    # repo-less Run works from its Run directory, and a base without its repo
    # (or the reverse) names a worktree that cannot exist.
    if (req.repo_path is None) != (req.base_branch is None):
      raise fastapi.HTTPException(
          status_code=400,
          detail=f"{req.task_type.value} delegations take repo_path and base_branch together; "
          "give both for a repo task, neither for a repo-less one")
  deps.require_found(await store.get_session(req.session_id))
  resolved_backend, resolved_model, backend_from_pool, route = await _authorize_spawn_request(req, store, task_mgr)
  return await _delegate_task_tree(
      req, task_mgr, events, caller, resolved_backend, resolved_model, backend_from_pool, route)


@router.post("/schedule-trigger")
async def schedule_trigger(
    req: models.ScheduleTriggerRequest,
    store: session_store.SessionStore = fastapi.Depends(deps.get_session_store),
    trigger_mgr: triggers.TriggerManager = fastapi.Depends(deps.get_trigger_manager),
) -> dict:
  """Schedule a delayed trigger that will wake the master CC after a delay."""
  deps.require_found(await store.get_session(req.session_id))

  if req.watch_targets is not None:
    if len(req.watch_targets) == 0:
      raise fastapi.HTTPException(status_code=400, detail="watch_targets must be non-empty when provided")
    for t in req.watch_targets:
      if t.kind == constants.WatchKind.SLURM_JOB:
        if t.job_id <= 0:
          raise fastapi.HTTPException(status_code=400, detail="watch_targets slurm job_id must be a positive integer")
      elif t.pid <= 0:
        raise fastapi.HTTPException(status_code=400, detail="watch_targets pids must be positive integers")

  watch_probe: dict[str, str] = {}
  try:
    trigger = await trigger_mgr.create_trigger(
        req.session_id,
        req.delay_seconds,
        req.message,
        watch_targets=req.watch_targets,
        probe_out=watch_probe,
    )
  except (triggers.RemoteVerifyError, triggers.ArchivedSessionError, triggers.PendingTriggerLimitError) as e:
    # Verify-on-create rejection, a target archived without a successor, or a
    # registration past the session's pending-trigger limit: surface as 422 so
    # the CLI exits with code 2.
    raise fastapi.HTTPException(status_code=422, detail=str(e)) from e
  except RuntimeError as e:
    raise deps.bad_request(e) from e
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
    trigger_mgr: triggers.TriggerManager = fastapi.Depends(deps.get_trigger_manager),
) -> dict:
  """Cancel a pending trigger."""
  try:
    await trigger_mgr.cancel_trigger(session_id, trigger_id)
  except FileNotFoundError as exc:
    raise fastapi.HTTPException(status_code=404, detail="Trigger not found") from exc
  return {"ok": True}


@router.post("/session-message")
async def session_message(
    req: models.SessionMessageRequest,
    store: session_store.SessionStore = fastapi.Depends(deps.get_session_store),
    task_mgr: task_sessions.TaskTreeManager = fastapi.Depends(deps.get_task_manager),
    caller: run_token.CallerIdentity = fastapi.Depends(deps.require_caller),
) -> dict:
  """Relay an agent message into another session's event log and wake its master.

  Persists an ``agent_message`` event (never a ``user`` event, so the runtime
  gate never counts it as a user message), then wakes the target master with
  the relay prefix. A mid-run target session enqueues the wake on the master
  work-item queue. An archived task target refuses the relay with 409
  (``task <id> is archived``);
  only the user's own message restores an archived node. A message over
  MAX_AGENT_MESSAGE_WEIGHTED_CHARS weighted characters
  (``text_length.weighted_length``) is refused with 422 before any lookup or write.
  """
  weighted_chars = text_length.weighted_length(req.content)
  if weighted_chars > constants.MAX_AGENT_MESSAGE_WEIGHTED_CHARS:
    log.info(
        "session_message_refused",
        session=req.session_id,
        target_session=req.target_session_id,
        weighted_chars=weighted_chars,
        limit=constants.MAX_AGENT_MESSAGE_WEIGHTED_CHARS,
    )
    raise fastapi.HTTPException(
        status_code=422,
        detail=(
            f"Message is {weighted_chars} weighted characters; "
            f"the limit is {constants.MAX_AGENT_MESSAGE_WEIGHTED_CHARS}.\n"
            "CJK characters, CJK punctuation, and fullwidth characters count 2.\n"
            "Write the long content to a file. Send a short message with the conclusion,\n"
            "the request, and the file path."))
  caller_meta = deps.require_found(await store.get_session(req.session_id))
  target = await store.get_session(req.target_session_id)
  if target is None:
    raise fastapi.HTTPException(status_code=404, detail="Target session not found")

  request_id = None
  if caller.claims is not None:
    request_id = control_events.derived_agent_send_request_id(
        caller.claims.run_id, "session send", req.target_session_id, req.content)

  try:
    await task_mgr.dispatch.admit_input(
        req.target_session_id,
        event_type=ET.AGENT_MESSAGE,
        content=req.content,
        actor="agent",
        request_id=request_id,
        from_session=caller_meta.id,
        from_session_name=caller_meta.name,
    )
    await task_mgr.dispatch.dispatch_pending(req.target_session_id)
  except (task_errors.TaskNotFoundError, task_errors.TaskForbiddenError, task_errors.TaskConflictError,
          task_errors.TaskInvalidError) as e:
    raise sessions._task_http_error(e) from e
  log.info(
      "session_message_dispatched",
      session=req.session_id,
      target_session=req.target_session_id,
      content_chars=len(req.content),
  )
  return {"status": "accepted"}
