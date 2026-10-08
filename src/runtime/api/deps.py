"""FastAPI dependency injection helpers.

Every ``get_*`` here is ``async def`` on purpose: FastAPI resolves a sync
dependency through a threadpool handoff per request (a thread round-trip plus
an event-loop wake per dependency per call), while a coroutine dependency is
awaited directly on the event loop. These getters only build or return a
process singleton, so the async form costs a dict check. The plain-name
``*_manager()`` functions are the sync forms for direct callers (startup and
the websocket handlers in server.py); the ``get_*`` names are the
Depends forms.
"""

import fastapi

from src.infra import config, constants, models
from src.runtime import run_token, runs, sessions, task_sessions, threads, triggers

# Module-level singletons (created once per process)
_session_manager: sessions.SessionManager | None = None
_thread_manager: threads.ThreadManager | None = None
_trigger_manager: triggers.TriggerManager | None = None
_task_manager: task_sessions.TaskTreeManager | None = None


def session_manager() -> sessions.SessionManager:
  global _session_manager
  if _session_manager is None:
    _session_manager = sessions.SessionManager(config.get_config())
  return _session_manager


async def get_session_manager() -> sessions.SessionManager:
  return session_manager()


def task_manager() -> task_sessions.TaskTreeManager:
  """The task-tree owner singleton; it owns the control lock the runs.RunStore shares.

  Construction installs the execution adapter as the input dispatcher's
  executor — the application initialization owner wiring durable dispatch to
  actual manager/worker/review execution. A test-built TaskTreeManager keeps
  its executor None until it installs one.
  """
  global _task_manager
  if _task_manager is None:
    _task_manager = task_sessions.TaskTreeManager(config.get_config(), session_manager())
    from src.runtime import task_execution
    _task_manager.dispatch.executor = task_execution.TaskExecutionAdapter(
        config.get_config(), session_manager(), _task_manager)
  return _task_manager


async def get_task_manager() -> task_sessions.TaskTreeManager:
  return task_manager()


def run_store() -> runs.RunStore:
  return task_manager().runs


async def get_run_store() -> runs.RunStore:
  return task_manager().runs


def set_task_manager(mgr: task_sessions.TaskTreeManager | None) -> None:
  """Replace the task-tree owner singleton (tests); None restores lazy construction."""
  global _task_manager
  _task_manager = mgr


def thread_manager() -> threads.ThreadManager:
  global _thread_manager
  if _thread_manager is None:
    _thread_manager = threads.ThreadManager(config.get_config())
  return _thread_manager


async def get_thread_manager() -> threads.ThreadManager:
  return thread_manager()


def trigger_manager() -> triggers.TriggerManager:
  global _trigger_manager
  if _trigger_manager is None:
    _trigger_manager = triggers.TriggerManager(config.get_config(), session_manager())
  return _trigger_manager


async def get_trigger_manager() -> triggers.TriggerManager:
  return trigger_manager()


def set_trigger_manager(mgr: triggers.TriggerManager) -> None:
  """Set the trigger manager singleton (called from server lifespan)."""
  global _trigger_manager
  _trigger_manager = mgr


async def get_config_on_loop() -> config.CharlieBotConfig:
  """Async config dependency for the polled routes.

  ``Depends(config.get_config)`` on the sync core reader pays the threadpool handoff
  described in the module docstring on every request; this resolves the
  memoized instance on the event loop instead.
  """
  return config.get_config()


# Client-visible 404 detail for an unresolvable session id: the sites that
# serve it (require_found and the fork/elone/delete raisers, the events
# viewer, and the Slack reply path via SlackReplyError) share this one
# spelling, and tests pin it. Distinct deliberate wordings exist elsewhere
# (e.g. internal.py's "Target session not found" for a wake's cross-session
# target); this constant homes only the unqualified one.
SESSION_NOT_FOUND_DETAIL = "Session not found"


def require_found(meta: models.SessionMetadata | None) -> models.SessionMetadata:
  """Return non-None session metadata, or raise 404 when the manager found no session."""
  if not meta:
    raise fastapi.HTTPException(status_code=404, detail=SESSION_NOT_FOUND_DETAIL)
  return meta


async def require_session(
    session_id: str,
    session_mgr: sessions.SessionManager = fastapi.Depends(get_session_manager),
) -> models.SessionMetadata:
  """Fetch a session or raise 404. Use as a FastAPI dependency."""
  return require_found(await session_mgr.get_session(session_id))


def bad_request(exc: Exception) -> fastapi.HTTPException:
  """Build the one expected-failure spelling: HTTP 400 whose detail is the exception's message.

  Route handlers raise this from the except clauses that translate a domain
  failure (the delegate spawn's backend resolution, the plan registry's
  lineage rules, fork/elone succession, cron-task backend validation, trigger
  scheduling, the openai-compatible proxy's request translation); raising
  keeps the ``from e`` chain intact.
  """
  return fastapi.HTTPException(status_code=400, detail=str(exc))


async def require_caller(
    request: fastapi.Request,
    run_store: runs.RunStore = fastapi.Depends(get_run_store),
) -> run_token.CallerIdentity:
  """Resolve the verified caller identity of a structural request.

  A bearer equal to the operator access key (or no bearer at all — the cookie
  the auth middleware already checked) is an operator caller. Any other bearer
  is a run token: it must verify against the operator signing key AND pass the
  one shared active-Run predicate (registered, no terminal fact, launched
  identity pinned — src.runtime.runs.run_identity_refusal), and it never falls
  back to the cookie or the access key. Run-token use with a missing signing
  key is an explicit 401.

  The ``X-CharlieBot-Caller-Session`` header is recorded only on operator
  identities — an operator claiming a session can at most lose its own wake —
  while an agent's session always comes from its verified token.
  """
  bearer = run_token.bearer_from_authorization(request.headers.get("authorization"))
  if not bearer:
    return run_token.CallerIdentity(
        kind="operator", session_id=request.headers.get(constants.CALLER_SESSION_HEADER) or None)
  key = config.configured_access_key()
  if key and bearer == key:
    return run_token.CallerIdentity(
        kind="operator", session_id=request.headers.get(constants.CALLER_SESSION_HEADER) or None)
  # Anything else is run-token use: fail closed, never fall back.
  if not key:
    raise fastapi.HTTPException(
        status_code=401,
        detail=f"run token presented ({constants.RUN_TOKEN_ENV}) but no signing key is configured",
    )
  try:
    claims = run_token.verify_run_token(bearer, key)
  except run_token.RunTokenError as e:
    raise fastapi.HTTPException(status_code=401, detail=f"invalid run token: {e}") from e
  run = await run_store.get_run(claims.session_id, claims.run_id)
  refusal = runs.run_identity_refusal(run, run_store.load_events_sync(claims.session_id))
  if refusal is not None:
    raise fastapi.HTTPException(status_code=401, detail=refusal)
  assert run is not None
  return run_token.CallerIdentity(kind="agent", claims=claims)
