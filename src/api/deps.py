"""FastAPI dependency injection helpers."""

from src.core.config import get_config
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
