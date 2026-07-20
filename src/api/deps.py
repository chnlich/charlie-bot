"""FastAPI dependency injection helpers."""

from fastapi import Depends, HTTPException

from src.core.config import get_config
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


def get_session_manager() -> SessionManager:
  global _session_manager
  if _session_manager is None:
    _session_manager = SessionManager(get_config())
  return _session_manager


def get_thread_manager() -> ThreadManager:
  global _thread_manager
  if _thread_manager is None:
    _thread_manager = ThreadManager(get_config())
  return _thread_manager


def get_trigger_manager() -> TriggerManager:
  global _trigger_manager
  if _trigger_manager is None:
    _trigger_manager = TriggerManager(get_config(), get_session_manager())
  return _trigger_manager


def set_trigger_manager(mgr: TriggerManager) -> None:
  """Set the trigger manager singleton (called from server lifespan)."""
  global _trigger_manager
  _trigger_manager = mgr


def get_plan_manager() -> PlanRegistryManager:
  global _plan_manager
  if _plan_manager is None:
    _plan_manager = PlanRegistryManager(get_config(), get_session_manager(), get_thread_manager())
  return _plan_manager


async def require_session(
    session_id: str,
    session_mgr: SessionManager = Depends(get_session_manager),
) -> SessionMetadata:
  """Fetch a session or raise 404. Use as a FastAPI dependency."""
  meta = await session_mgr.get_session(session_id)
  if not meta:
    raise HTTPException(status_code=404, detail="Session not found")
  return meta
