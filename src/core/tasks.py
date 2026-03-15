"""Helpers for fire-and-forget asyncio tasks with exception logging."""

import asyncio
from typing import Coroutine, Optional

import structlog

log = structlog.get_logger()


def _task_done_callback(task: asyncio.Task) -> None:
  """Log any exception from a completed task instead of silently discarding it."""
  if task.cancelled():
    return
  exc = task.exception()
  if exc is not None:
    log.error("background_task_failed", task_name=task.get_name(), exc_info=exc)


def create_logged_task(coro: Coroutine, *, name: Optional[str] = None) -> asyncio.Task:
  """Create an asyncio task with an exception-logging done callback.

  Drop-in replacement for asyncio.create_task() that prevents silent failures
  in fire-and-forget background tasks.
  """
  task = asyncio.create_task(coro, name=name)
  task.add_done_callback(_task_done_callback)
  return task
