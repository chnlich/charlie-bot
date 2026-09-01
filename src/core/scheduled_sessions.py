"""Scheduled-session backend rotation, succession bookkeeping, and task-yaml backend persistence."""

import asyncio
from typing import Any

import structlog

from src.core.config import cron_path
from src.core.models import (
  CreateSessionRequest,
  SessionMetadata,
  SessionStatus,
  utc_now,
)
from src.core.yaml_utils import load_yaml, save_yaml

log = structlog.get_logger()


class ScheduledSessionBusyError(RuntimeError):
  """Raised when a scheduled session cannot rotate backends because it is busy."""


class ScheduledSessionStore:
  """Scheduled-session rotation operations."""

  def __init__(self, session_manager: Any):
    self._session_manager = session_manager

  async def ensure_scheduled_session_backend(
      self,
      task_name: str,
      backend: str,
      session_cache: dict[str, list[SessionMetadata]] | None = None,
      skip_if_busy: bool = False,
      role: str | None = None,
      group: str | None = None,
  ) -> SessionMetadata | None:
    """Return the active scheduled session for task_name/backend, rotating history if needed.

    Backend changes are generation changes: the old active session is archived and a new
    scheduled session is created with only scheduler bookkeeping copied over. ``role`` and
    ``group`` (passed for mode: master PM tasks) ride along on both creation paths — first
    creation and generation rotation alike.
    """
    active_sessions = await self._active_scheduled_sessions(task_name, session_cache)
    for session in active_sessions:
      if session.backend == backend:
        return session

    old_session = active_sessions[0] if active_sessions else None
    if old_session is None:
      meta = await self._session_manager.create_session(
          CreateSessionRequest(name=f"Scheduled: {task_name}", scheduled_task=task_name, role=role),
          backend=backend)
      if group is not None:
        meta.group = group
        meta.updated_at = utc_now()
        await self._session_manager.save_metadata(meta)
      log.info("scheduled_session_created", task=task_name, session=meta.id, backend=backend)
      if session_cache is not None:
        session_cache.setdefault(task_name, []).insert(0, meta)
      return meta

    if await self._scheduled_session_busy(old_session):
      message = (
          f"scheduled task '{task_name}' backend switch from '{old_session.backend}' to '{backend}' is blocked "
          f"because session '{old_session.id}' has running work")
      if skip_if_busy:
        log.warning(
            "scheduled_session_rotation_busy",
            task=task_name,
            session=old_session.id,
            old_backend=old_session.backend,
            backend=backend,
        )
        return None
      raise ScheduledSessionBusyError(message)

    await self._session_manager.archive_session(old_session.id)
    meta = await self._session_manager.create_session(
        CreateSessionRequest(name=f"Scheduled: {task_name}", scheduled_task=task_name, role=role),
        backend=backend)
    self.migrate_scheduler_bookkeeping(old_session, meta)
    if group is not None:
      meta.group = group
    meta.updated_at = utc_now()
    await self._session_manager.save_metadata(meta)
    if session_cache is not None:
      cached_sessions = session_cache.setdefault(task_name, [])
      for cached in cached_sessions:
        if cached.id == old_session.id:
          cached.status = SessionStatus.ARCHIVED
      cached_sessions.insert(0, meta)
    log.info(
        "scheduled_session_rotated",
        task=task_name,
        old_session=old_session.id,
        new_session=meta.id,
        old_backend=old_session.backend,
        backend=backend,
    )
    return meta

  async def _active_scheduled_sessions(
      self,
      task_name: str,
      session_cache: dict[str, list[SessionMetadata]] | None = None,
  ) -> list[SessionMetadata]:
    """Return active scheduled sessions for task_name, newest first."""
    if session_cache is not None:
      candidates = session_cache.get(task_name, [])
    else:
      candidates = await self._session_manager.list_sessions(status=SessionStatus.ACTIVE, scheduled=True)
    sessions = [
        session for session in candidates
        if session.scheduled_task == task_name and session.status == SessionStatus.ACTIVE
    ]
    sessions.sort(key=lambda session: session.updated_at, reverse=True)
    return sessions

  async def _scheduled_session_busy(self, session: SessionMetadata) -> bool:
    """Return whether a scheduled session has active master thinking or worker threads."""
    return bool(session.thinking_since) or await self._session_manager._has_running_tasks(session.id)

  def migrate_scheduler_bookkeeping(self, old_session: SessionMetadata, new_session: SessionMetadata) -> None:
    """Carry scheduler bookkeeping fields onto the task's next generation.

    The single home of the migration field list: both the rotation body above
    and the elone succession branch (src/core/sessions.py) call it, so what
    migrates on a generation change is defined exactly once.
    """
    new_session.last_scheduled_run = old_session.last_scheduled_run
    new_session.last_scheduled_cron = old_session.last_scheduled_cron
    new_session.last_run_status = old_session.last_run_status

  async def write_scheduled_task_backend(self, task_name: str, backend: str) -> None:
    """Write only the ``backend`` key of *task_name*'s cron yaml, preserving every other key.

    Full-file rewrite via save_yaml — the same persistence form as the cron
    editor's whole-record update. Path resolution comes from the canonical
    helper (src.core.config.cron_path); a missing, empty, or non-mapping task
    file fails loud instead of silently recreating one.
    """
    await asyncio.to_thread(self._write_scheduled_task_backend_sync, task_name, backend)

  @staticmethod
  def _write_scheduled_task_backend_sync(task_name: str, backend: str) -> None:
    path = cron_path(task_name)
    data = load_yaml(path)
    if not isinstance(data, dict):
      raise FileNotFoundError(f"scheduled task '{task_name}' has no readable cron yaml at {path}")
    data["backend"] = backend
    save_yaml(path, data)
