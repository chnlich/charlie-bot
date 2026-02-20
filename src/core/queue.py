"""Deterministic task queue manager for CharlieBot.

No LLM involvement. Pure Python priority queue with disk persistence.
All writes are atomic to survive crashes without corruption.
"""

import asyncio
import json
import os
from datetime import datetime
from pathlib import Path
from typing import Optional

import aiofiles
import structlog

from src.core.config import CharlieBotConfig
from src.core.models import Priority, Task, TaskQueue, TaskStatus

log = structlog.get_logger()

# Priority ordering: P0 is highest (lowest integer value for sorting)
_PRIORITY_ORDER = {Priority.P0: 0, Priority.P1: 1, Priority.P2: 2}


class QueueManager:
  """
  Manages the task queue for one session.
  Handles push/pop/reorder and spawns Workers via the provided factory.
  """

  def __init__(self, session_id: str, cfg: CharlieBotConfig):
    self.session_id = session_id
    self._queue_path = cfg.sessions_dir / session_id / "task_queue.json"
    self._lock = asyncio.Lock()

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
    queue.updated_at = datetime.utcnow()
    tmp = self._queue_path.with_suffix(".tmp")
    async with aiofiles.open(tmp, "w") as f:
      await f.write(queue.model_dump_json(indent=2))
    os.replace(tmp, self._queue_path)

def _sort_tasks(queue: TaskQueue) -> None:
  """Sort tasks in-place: P0 > P1 > P2, then by created_at ascending."""
  queue.tasks.sort(key=lambda t: (_PRIORITY_ORDER.get(t.priority, 99), t.created_at))
