"""Per-session queue dispatcher: pops tasks, creates threads, and spawns workers."""

import asyncio
from typing import Optional

import structlog

from src.agents.worker import QuotaExhaustedException, Worker
from src.core.models import Task, ThreadStatus
from src.core.queue import QueueManager
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager
from src.core.config import CharliBotConfig

log = structlog.get_logger()

# Idle polls before the loop exits (each poll sleeps 2 s → exits after ~6 s of idle)
_IDLE_EXIT_THRESHOLD = 3


class SessionDispatcher:
  """
  Owns the background queue loop for one session.
  Pops tasks, creates Thread entries (visible in the UI), and spawns Workers.
  """

  def __init__(
    self,
    session_id: str,
    cfg: CharliBotConfig,
    session_mgr: SessionManager,
    thread_mgr: ThreadManager,
  ):
    self._session_id = session_id
    self._queue_mgr = QueueManager(session_id, cfg)
    self._session_mgr = session_mgr
    self._thread_mgr = thread_mgr
    self._semaphore = asyncio.Semaphore(cfg.max_concurrent_workers)
    self._loop_task: Optional[asyncio.Task] = None

  async def enqueue(self, task: Task) -> None:
    """Push task to the queue and ensure the background loop is running."""
    await self._queue_mgr.push(task)
    if self._loop_task is None or self._loop_task.done():
      self._loop_task = asyncio.create_task(self._loop())
      log.info("dispatcher_loop_started", session=self._session_id)

  # ---------------------------------------------------------------------------
  # Background loop
  # ---------------------------------------------------------------------------

  async def _loop(self) -> None:
    """Pop and dispatch tasks until the queue is idle for _IDLE_EXIT_THRESHOLD polls."""
    idle_count = 0
    while True:
      task = await self._queue_mgr.pop_next()
      if task is None:
        idle_count += 1
        if idle_count >= _IDLE_EXIT_THRESHOLD:
          log.info("dispatcher_loop_idle_exit", session=self._session_id)
          return
        await asyncio.sleep(2)
        continue
      idle_count = 0
      await self._semaphore.acquire()
      asyncio.create_task(self._run_task(task))

  async def _run_task(self, task: Task) -> None:
    """Spawn a Worker for the task's thread and update statuses on the way."""
    try:
      session_meta = await self._session_mgr.get_session(self._session_id)
      if not session_meta:
        log.error("dispatcher_session_missing", session=self._session_id, task_id=task.id)
        await self._queue_mgr.mark_failed(task.id)
        return

      # Use existing thread (created inline in chat.py) or create one
      if task.thread_id:
        thread = await self._thread_mgr.get_thread(self._session_id, task.thread_id)
      else:
        thread = None
      if not thread:
        thread = await self._thread_mgr.create_thread(session_meta, task)

      await self._thread_mgr.update_status(self._session_id, thread.id, ThreadStatus.RUNNING)
      log.info("thread_running", thread_id=thread.id, task_id=task.id)

      # Build and run Worker
      events_log = await self._thread_mgr.get_events_log_path(self._session_id, thread.id)
      worktree = await self._thread_mgr.get_worktree_path(self._session_id, thread.id)
      worker = Worker(thread, worktree, events_log, task.description)

      try:
        await worker.run()
        await self._queue_mgr.mark_complete(task.id)
        await self._thread_mgr.update_status(
          self._session_id, thread.id, ThreadStatus.COMPLETED, exit_code=0
        )
        log.info("task_completed", task_id=task.id, thread_id=thread.id)

      except QuotaExhaustedException:
        await self._queue_mgr.mark_pending_quota(task.id)
        await self._thread_mgr.update_status(self._session_id, thread.id, ThreadStatus.FAILED)
        log.warning("task_quota_exhausted", task_id=task.id)

      except Exception as e:
        await self._queue_mgr.mark_failed(task.id)
        await self._thread_mgr.update_status(self._session_id, thread.id, ThreadStatus.FAILED)
        log.error("task_failed", task_id=task.id, error=str(e))

    finally:
      self._semaphore.release()


# ---------------------------------------------------------------------------
# Global registry — one SessionDispatcher per session
# ---------------------------------------------------------------------------

_dispatchers: dict[str, SessionDispatcher] = {}


def get_or_create(
  session_id: str,
  cfg: CharliBotConfig,
  session_mgr: SessionManager,
  thread_mgr: ThreadManager,
) -> SessionDispatcher:
  if session_id not in _dispatchers:
    _dispatchers[session_id] = SessionDispatcher(session_id, cfg, session_mgr, thread_mgr)
  return _dispatchers[session_id]
