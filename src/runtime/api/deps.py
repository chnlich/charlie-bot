"""FastAPI dependency injection helpers."""

import fastapi

from src.infra import config, constants, models
from src.runtime import (
    run_token,
    runs,
    session_anchors,
    session_events,
    session_fork,
    session_lifecycle,
    session_listing,
    session_search,
    session_sidebar,
    session_store,
    session_successor,
    task_execution,
    task_sessions,
    triggers,
)


async def get_session_store() -> session_store.SessionStore:
  return session_store.store()


async def get_session_events() -> session_events.SessionEvents:
  return session_events.events()


async def get_session_anchors() -> session_anchors.SessionAnchors:
  return session_anchors.anchors()


async def get_session_fork() -> session_fork.SessionFork:
  return session_fork.fork()


async def get_session_lifecycle() -> session_lifecycle.SessionLifecycle:
  return session_lifecycle.lifecycle()


async def get_session_listing() -> session_listing.SessionListing:
  return session_listing.listing()


async def get_session_search() -> session_search.SessionSearch:
  return session_search.search()


async def get_session_successor() -> session_successor.SessionSuccessor:
  return session_successor.successor()


async def get_session_sidebar() -> session_sidebar.SessionSidebar:
  return session_sidebar.sidebar()


async def get_task_manager() -> task_sessions.TaskTreeManager:
  return task_execution.task_manager()


async def get_run_store() -> runs.RunStore:
  return task_execution.task_manager().runs


def get_trigger_manager() -> triggers.TriggerManager:
  return triggers.trigger_manager()


def get_config_on_loop() -> config.CharlieBotConfig:
  """The memoized config as a Depends target."""
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
    store: session_store.SessionStore = fastapi.Depends(get_session_store),
) -> models.SessionMetadata:
  """Fetch a session or raise 404. Use as a FastAPI dependency."""
  return require_found(await store.get_session(session_id))


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
