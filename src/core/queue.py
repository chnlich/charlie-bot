"""Deterministic task queue manager for CharlieBot.

No LLM involvement. Pure Python priority queue with disk persistence.
All writes are atomic to survive crashes without corruption.
"""

import asyncio
import json
import os
from pathlib import Path
from typing import Callable, Optional

import aiofiles
import structlog

from src.core.config import CharliBotConfig
from src.core.models import Priority, Task, TaskQueue, TaskStatus

log = structlog.get_logger()

# Priority ordering: P0 is highest (lowest integer value for sorting)
_PRIORITY_ORDER = {Priority.P0: 0, Priority.P1: 1, Priority.P2: 2}


class QueueManager:
  """
  Manages the task queue for one session.
  Handles push/pop/reorder and spawns Workers via the provided factory.
  """

  def __init__(self, session_id: str, cfg: CharliBotConfig):
    self.session_id = session_id
    self._queue_path = cfg.sessions_dir / session_id / "task_queue.json"
    self._lock = asyncio.Lock()
    self._semaphore = asyncio.Semaphore(cfg.max_concurrent_workers)
    self._running = False

  # ---------------------------------------------------------------------------
  # Queue operations (all hold _lock for thread safety)
  # ---------------------------------------------------------------------------

  async def push(self, task: Task) -> None:
    """Add a task to the queue and sort by priority."""
    async with self._lock:
      queue = await self._load()
      queue.tasks.append(task)
      _sort_tasks(queue)
      await self._save(queue)
    log.info("task_queued", task_id=task.id, priority=task.priority, session=self.session_id)

  async def pop_next(self) -> Optional[Task]:
    """Pop the highest-priority PENDING task. Returns None if queue is empty."""
    async with self._lock:
      queue = await self._load()
      for task in queue.tasks:
        if task.status == TaskStatus.PENDING:
          task.status = TaskStatus.RUNNING
          await self._save(queue)
          return task
    return None

  async def reorder(self, task_id: str, new_priority: Priority) -> None:
    """Change a task's priority and re-sort the queue."""
    async with self._lock:
      queue = await self._load()
      for task in queue.tasks:
        if task.id == task_id and task.status == TaskStatus.PENDING:
          task.priority = new_priority
          break
      _sort_tasks(queue)
      await self._save(queue)

  async def cancel(self, task_id: str) -> None:
    """Mark a queued task as cancelled."""
    async with self._lock:
      queue = await self._load()
      for task in queue.tasks:
        if task.id == task_id and task.status == TaskStatus.PENDING:
          task.status = TaskStatus.CANCELLED
          break
      await self._save(queue)

  async def mark_complete(self, task_id: str) -> None:
    await self._update_status(task_id, TaskStatus.COMPLETED)

  async def mark_failed(self, task_id: str) -> None:
    await self._update_status(task_id, TaskStatus.FAILED)

  async def mark_pending_quota(self, task_id: str) -> None:
    await self._update_status(task_id, TaskStatus.PENDING_QUOTA)

  async def requeue_pending_quota(self) -> int:
    """Re-enqueue any PENDING_QUOTA tasks as PENDING. Returns count re-queued."""
    async with self._lock:
      queue = await self._load()
      count = 0
      for task in queue.tasks:
        if task.status == TaskStatus.PENDING_QUOTA:
          task.status = TaskStatus.PENDING
          count += 1
      if count:
        _sort_tasks(queue)
        await self._save(queue)
    return count

  async def get_queue(self) -> TaskQueue:
    async with self._lock:
      return await self._load()

  async def _update_status(self, task_id: str, status: TaskStatus) -> None:
    async with self._lock:
      queue = await self._load()
      for task in queue.tasks:
        if task.id == task_id:
          task.status = status
          break
      await self._save(queue)

  # ---------------------------------------------------------------------------
  # Disk persistence
  # ---------------------------------------------------------------------------

  async def _load(self) -> TaskQueue:
    """Load task_queue.json from disk. Creates empty queue if missing."""
    if not self._queue_path.exists():
      return TaskQueue(session_id=self.session_id)
    async with aiofiles.open(self._queue_path, "r") as f:
      raw = await f.read()
    return TaskQueue.model_validate_json(raw)

  async def _save(self, queue: TaskQueue) -> None:
    """Atomically save queue to disk using a temp file + os.replace()."""
    from datetime import datetime
    queue.updated_at = datetime.utcnow()
    tmp = self._queue_path.with_suffix(".tmp")
    async with aiofiles.open(tmp, "w") as f:
      await f.write(queue.model_dump_json(indent=2))
    os.replace(tmp, self._queue_path)

  # ---------------------------------------------------------------------------
  # Worker loop
  # ---------------------------------------------------------------------------

  async def run_loop(self, worker_factory: Callable) -> None:
    """
    Continuous loop that pops tasks and spawns Workers.
    Runs until stop() is called.
    """
    self._running = True
    log.info("queue_loop_started", session=self.session_id)
    while self._running:
      await self._semaphore.acquire()
      task = await self.pop_next()
      if task is None:
        self._semaphore.release()
        await asyncio.sleep(2)
        continue
      log.info("task_dispatched", task_id=task.id, priority=task.priority)
      asyncio.create_task(self._run_worker(task, worker_factory))

  def stop(self) -> None:
    """Signal the loop to stop after the current iteration."""
    self._running = False

  async def _run_worker(self, task: Task, worker_factory: Callable) -> None:
    """Run one worker for one task, release semaphore slot when done."""
    try:
      worker = worker_factory(task)
      await worker.run()
      await self.mark_complete(task.id)
      log.info("task_completed", task_id=task.id)
    except Exception as e:
      error_name = type(e).__name__
      if error_name == "QuotaExhaustedException":
        await self.mark_pending_quota(task.id)
        log.warning("task_quota_exhausted", task_id=task.id)
      else:
        await self.mark_failed(task.id)
        log.error("task_failed", task_id=task.id, error=str(e))
    finally:
      self._semaphore.release()


def _sort_tasks(queue: TaskQueue) -> None:
  """Sort tasks in-place: P0 > P1 > P2, then by created_at ascending."""
  queue.tasks.sort(key=lambda t: (_PRIORITY_ORDER.get(t.priority, 99), t.created_at))
