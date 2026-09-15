"""FastAPI dependency injection helpers.

Every ``get_*`` here is ``async def`` on purpose: FastAPI resolves a sync
dependency through a threadpool handoff per request (a thread round-trip plus
an event-loop wake per dependency per call), while a coroutine dependency is
awaited directly on the event loop. These getters only build or return a
process singleton, so the async form costs a dict check. The plain-name
``*_manager()`` functions are the sync forms for direct callers (startup and
the websocket handlers in server.py and tui.py); the ``get_*`` names are the
Depends forms.
"""

from fastapi import Depends, HTTPException, Request

from src.core.config import CharlieBotConfig, configured_access_key, get_config
from src.core.models import SessionMetadata
from src.core.plans import PlanRegistryManager
from src.core.run_token import (
    RUN_TOKEN_ENV,
    CallerIdentity,
    RunTokenError,
    bearer_from_authorization,
    verify_run_token,
)
from src.core.runs import RunStore, run_identity_refusal
from src.core.sessions import SessionManager
from src.core.task_sessions import TaskTreeManager
from src.core.threads import ThreadManager
from src.core.triggers import TriggerManager

# Module-level singletons (created once per process)
_session_manager: SessionManager | None = None
_thread_manager: ThreadManager | None = None
_trigger_manager: TriggerManager | None = None
_plan_manager: PlanRegistryManager | None = None
_task_manager: TaskTreeManager | None = None


def session_manager() -> SessionManager:
  global _session_manager
  if _session_manager is None:
    _session_manager = SessionManager(get_config())
  return _session_manager


async def get_session_manager() -> SessionManager:
  return session_manager()


def task_manager() -> TaskTreeManager:
  """The task-tree owner singleton; it owns the control lock the RunStore shares.

  Construction installs the execution adapter as the input dispatcher's
  executor — the application initialization owner wiring durable dispatch to
  actual manager/worker/review execution. A test-built TaskTreeManager keeps
  its executor None until it installs one.
  """
  global _task_manager
  if _task_manager is None:
    _task_manager = TaskTreeManager(get_config(), session_manager())
    from src.core.task_execution import TaskExecutionAdapter
    _task_manager.dispatch.executor = TaskExecutionAdapter(
        get_config(), session_manager(), _task_manager)
  return _task_manager


async def get_task_manager() -> TaskTreeManager:
  return task_manager()


def run_store() -> RunStore:
  return task_manager().runs


async def get_run_store() -> RunStore:
  return task_manager().runs


def set_task_manager(mgr: TaskTreeManager | None) -> None:
  """Replace the task-tree owner singleton (tests); None restores lazy construction."""
  global _task_manager
  _task_manager = mgr


def thread_manager() -> ThreadManager:
  global _thread_manager
  if _thread_manager is None:
    _thread_manager = ThreadManager(get_config())
  return _thread_manager


async def get_thread_manager() -> ThreadManager:
  return thread_manager()


def trigger_manager() -> TriggerManager:
  global _trigger_manager
  if _trigger_manager is None:
    _trigger_manager = TriggerManager(get_config(), session_manager())
  return _trigger_manager


async def get_trigger_manager() -> TriggerManager:
  return trigger_manager()


def set_trigger_manager(mgr: TriggerManager) -> None:
  """Set the trigger manager singleton (called from server lifespan)."""
  global _trigger_manager
  _trigger_manager = mgr


def plan_manager() -> PlanRegistryManager:
  global _plan_manager
  if _plan_manager is None:
    _plan_manager = PlanRegistryManager(get_config(), session_manager())
  return _plan_manager


async def get_plan_manager() -> PlanRegistryManager:
  return plan_manager()


async def get_config_on_loop() -> CharlieBotConfig:
  """Async config dependency for the polled routes.

  ``Depends(get_config)`` on the sync core reader pays the threadpool handoff
  described in the module docstring on every request; this resolves the
  memoized instance on the event loop instead.
  """
  return get_config()


# Client-visible 404 detail for an unresolvable session id: the sites that
# serve it (require_found and the fork/elone/delete raisers, the events
# viewer, and the Slack reply path via SlackReplyError) share this one
# spelling, and tests pin it. Distinct deliberate wordings exist elsewhere
# (e.g. internal.py's "Target session not found" for a wake's cross-session
# target); this constant homes only the unqualified one.
SESSION_NOT_FOUND_DETAIL = "Session not found"


def require_found(meta: SessionMetadata | None) -> SessionMetadata:
  """Return non-None session metadata, or raise 404 when the manager found no session."""
  if not meta:
    raise HTTPException(status_code=404, detail=SESSION_NOT_FOUND_DETAIL)
  return meta


async def require_session(
    session_id: str,
    session_mgr: SessionManager = Depends(get_session_manager),
) -> SessionMetadata:
  """Fetch a session or raise 404. Use as a FastAPI dependency."""
  return require_found(await session_mgr.get_session(session_id))


def bad_request(exc: Exception) -> HTTPException:
  """Build the one expected-failure spelling: HTTP 400 whose detail is the exception's message.

  Route handlers raise this from the except clauses that translate a domain
  failure (the delegate spawn's backend resolution, the plan registry's
  lineage rules, fork/elone succession, cron-task backend validation, trigger
  scheduling, the openai-compatible proxy's request translation); raising
  keeps the ``from e`` chain intact.
  """
  return HTTPException(status_code=400, detail=str(exc))


async def require_caller(
    request: Request,
    run_store: RunStore = Depends(get_run_store),
) -> CallerIdentity:
  """Resolve the verified caller identity of a structural request.

  A bearer equal to the operator access key (or no bearer at all — the cookie
  the auth middleware already checked) is an operator caller. Any other bearer
  is a run token: it must verify against the operator signing key AND pass the
  one shared active-Run predicate (registered, no terminal fact, launched
  identity pinned — src.core.runs.run_identity_refusal), and it never falls
  back to the cookie or the access key. Run-token use with a missing signing
  key is an explicit 401.
  """
  bearer = bearer_from_authorization(request.headers.get("authorization"))
  if not bearer:
    return CallerIdentity(kind="operator")
  key = configured_access_key()
  if key and bearer == key:
    return CallerIdentity(kind="operator")
  # Anything else is run-token use: fail closed, never fall back.
  if not key:
    raise HTTPException(
        status_code=401,
        detail=f"run token presented ({RUN_TOKEN_ENV}) but no signing key is configured",
    )
  try:
    claims = verify_run_token(bearer, key)
  except RunTokenError as e:
    raise HTTPException(status_code=401, detail=f"invalid run token: {e}") from e
  run = await run_store.get_run(claims.session_id, claims.run_id)
  refusal = run_identity_refusal(run, run_store.load_events_sync(claims.session_id))
  if refusal is not None:
    raise HTTPException(status_code=401, detail=refusal)
  assert run is not None
  return CallerIdentity(kind="agent", claims=claims)
