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

from fastapi import Depends, HTTPException

from src.core.config import CharlieBotConfig, get_config
from src.core.models import SessionMetadata
from src.core.plans import PlanRegistryManager
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager
from src.core.triggers import TriggerManager

# Module-level singletons (created once per process)
_session_manager: SessionManager | None = None
_thread_manager: ThreadManager | None = None
_trigger_manager: TriggerManager | None = None
_plan_manager: PlanRegistryManager | None = None


def session_manager() -> SessionManager:
  global _session_manager
  if _session_manager is None:
    _session_manager = SessionManager(get_config())
  return _session_manager


async def get_session_manager() -> SessionManager:
  return session_manager()


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


def require_found(meta: SessionMetadata | None) -> SessionMetadata:
  """Return non-None session metadata, or raise 404 when the manager found no session."""
  if not meta:
    raise HTTPException(status_code=404, detail="Session not found")
  return meta


async def require_session(
    session_id: str,
    session_mgr: SessionManager = Depends(get_session_manager),
) -> SessionMetadata:
  """Fetch a session or raise 404. Use as a FastAPI dependency."""
  return require_found(await session_mgr.get_session(session_id))
