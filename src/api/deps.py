"""FastAPI dependency injection helpers."""

from typing import Annotated

from fastapi import Depends, Path

from src.core.config import CharliBotConfig, get_config
from src.core.git import GitManager
from src.core.memory import MemoryManager
from src.core.queue import QueueManager
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager
from src.agents.master_agent import MasterAgent

# Module-level singletons (created once per process)
_git_manager: GitManager | None = None
_memory_manager: MemoryManager | None = None
_session_manager: SessionManager | None = None
_thread_manager: ThreadManager | None = None
_master_agent: MasterAgent | None = None


def _get_git_manager() -> GitManager:
  global _git_manager
  if _git_manager is None:
    _git_manager = GitManager()
  return _git_manager


def _get_memory_manager() -> MemoryManager:
  global _memory_manager
  if _memory_manager is None:
    _memory_manager = MemoryManager(get_config())
  return _memory_manager


def get_session_manager() -> SessionManager:
  global _session_manager
  if _session_manager is None:
    _session_manager = SessionManager(get_config(), _get_git_manager())
  return _session_manager


def get_thread_manager() -> ThreadManager:
  global _thread_manager
  if _thread_manager is None:
    _thread_manager = ThreadManager(get_config(), _get_git_manager())
  return _thread_manager


def get_master_agent() -> MasterAgent:
  global _master_agent
  if _master_agent is None:
    _master_agent = MasterAgent(get_config(), _get_memory_manager())
  return _master_agent


def get_queue_manager(session_id: str = Path(...)) -> QueueManager:
  """Returns a QueueManager bound to the given session_id."""
  return QueueManager(session_id, get_config())


def get_memory() -> MemoryManager:
  return _get_memory_manager()
