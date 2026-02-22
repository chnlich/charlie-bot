"""FastAPI dependency injection helpers."""

from fastapi import Depends, HTTPException

from src.core.config import get_config
from src.core.models import SessionMetadata
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager

# Module-level singletons (created once per process)
_session_manager: SessionManager | None = None
_thread_manager: ThreadManager | None = None


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


async def require_session(
    session_id: str,
    session_mgr: SessionManager = Depends(get_session_manager),
) -> SessionMetadata:
  """Fetch a session or raise 404. Use as a FastAPI dependency."""
  meta = await session_mgr.get_session(session_id)
  if not meta:
    raise HTTPException(status_code=404, detail="Session not found")
  return meta
