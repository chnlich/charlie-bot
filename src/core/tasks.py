"""Helpers for background asyncio tasks: fire-and-forget creation with exception
logging, and shutdown-time cancellation with a drain."""

import asyncio
import contextlib
from collections.abc import Coroutine

from src.core.log_once import LazyStructlogLogger

log = LazyStructlogLogger()

# Keep strong references to background tasks so they aren't garbage collected mid-execution.
_background_tasks: set[asyncio.Task] = set()


def _task_done_callback(task: asyncio.Task) -> None:
  """Log any exception from a completed task instead of silently discarding it."""
  _background_tasks.discard(task)
  if task.cancelled():
    return
  exc = task.exception()
  if exc is not None:
    log.error("background_task_failed", task_name=task.get_name(), exc_info=exc)


def create_logged_task(coro: Coroutine, *, name: str | None = None) -> asyncio.Task:
  """Create an asyncio task with an exception-logging done callback.

  Drop-in replacement for asyncio.create_task() that prevents silent failures
  in fire-and-forget background tasks and avoids mid-execution garbage collection.
  """
  task = asyncio.create_task(coro, name=name)
  _background_tasks.add(task)
  task.add_done_callback(_task_done_callback)
  return task


async def cancel_and_wait(task: asyncio.Task | None) -> None:
  """Cancel *task*, then wait out the cancellation before returning; a None task is already quiet.

  Awaiting (suppressing CancelledError) lets the task's own finally block
  finish first, so a stopped task never outlives the shutdown step that
  stopped it.
  """
  if task is None:
    return
  task.cancel()
  with contextlib.suppress(asyncio.CancelledError):
    await task
