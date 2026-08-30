"""Thread management for CharlieBot Worker tasks."""

import asyncio
from datetime import datetime
from pathlib import Path

import aiofiles
import structlog

from src.core.config import CharlieBotConfig
from src.core.models import (
  TERMINAL_THREAD_STATUSES,
  SessionMetadata,
  TaskType,
  ThreadMetadata,
  ThreadStatus,
  utc_now,
)
from src.core.sidebar_state import mark_sidebar_dirty

log = structlog.get_logger()


def thread_events_log_path(session_dir: Path, thread_id: str) -> Path:
  """Return the path to a thread's events.jsonl under its session directory."""
  return session_dir / "threads" / thread_id / "data" / "events.jsonl"


class ThreadManager:
  """Creates and manages Worker threads."""

  def __init__(self, cfg: CharlieBotConfig):
    self._cfg = cfg

  async def create_thread(
      self,
      session_meta: SessionMetadata,
      description: str,
      branch_name: str | None = None,
      review_of: str | None = None,
      context: str | None = None,
      require_review: bool = True,
      task_type: TaskType | None = None,
  ) -> ThreadMetadata:
    """Create a new thread directory and metadata."""
    thread = ThreadMetadata(
        session_id=session_meta.id,
        description=description,
        branch_name=branch_name,
        review_of=review_of,
        context=context,
        require_review=require_review,
        task_type=task_type,
    )

    thread_dir = self.thread_dir(session_meta.id, thread.id)
    (thread_dir / "data").mkdir(parents=True, exist_ok=True)

    await self._save_metadata(thread)
    log.info("thread_created", thread_id=thread.id)
    return thread

  async def get_thread(self, session_id: str, thread_id: str) -> ThreadMetadata | None:
    path = self._metadata_path(session_id, thread_id)
    if not path.exists():
      return None
    async with aiofiles.open(path) as f:
      raw = await f.read()
    return ThreadMetadata.model_validate_json(raw)

  async def list_threads(self, session_id: str) -> list[ThreadMetadata]:
    threads_dir = self._cfg.sessions_dir / session_id / "threads"
    dirs = await asyncio.to_thread(
        lambda: [d for d in threads_dir.iterdir() if d.is_dir()] if threads_dir.exists() else [])
    all_meta = await asyncio.gather(*(self.get_thread(session_id, d.name) for d in dirs))
    threads = [m for m in all_meta if m]
    threads.sort(key=lambda t: t.created_at, reverse=True)
    return threads

  async def update_status(
      self,
      session_id: str,
      thread_id: str,
      status: ThreadStatus,
      pid: int | None = None,
      exit_code: int | None = None,
      completed_at: datetime | None = None,
  ) -> None:
    meta = await self.get_thread(session_id, thread_id)
    if not meta:
      return
    meta.status = status
    if pid is not None:
      meta.pid = pid
    if exit_code is not None:
      meta.exit_code = exit_code
    if status == ThreadStatus.RUNNING and not meta.started_at:
      meta.started_at = utc_now()
    if status in TERMINAL_THREAD_STATUSES:
      # An explicit completion time (e.g. the raw log's final mtime, which is
      # independent of when finalization happens to run) wins over "now".
      meta.completed_at = completed_at or utc_now()
    await self._save_metadata(meta)

  def thread_dir(self, session_id: str, thread_id: str) -> Path:
    """A thread's canonical on-disk directory (metadata.json and data/)."""
    return self._cfg.sessions_dir / session_id / "threads" / thread_id

  async def get_events_log_path(self, session_id: str, thread_id: str) -> Path:
    return thread_events_log_path(self._cfg.sessions_dir / session_id, thread_id)

  async def save_metadata(self, meta: ThreadMetadata) -> None:
    """Persist thread metadata to disk."""
    await self._save_metadata(meta)

  # ---------------------------------------------------------------------------
  # Private helpers
  # ---------------------------------------------------------------------------

  async def _save_metadata(self, meta: ThreadMetadata) -> None:
    path = self._metadata_path(meta.session_id, meta.id)
    path.parent.mkdir(parents=True, exist_ok=True)
    async with aiofiles.open(path, "w") as f:
      await f.write(meta.model_dump_json(indent=2))
    # Single funnel behind create_thread/update_status/save_metadata: thread
    # status transitions (running -> terminal) land here.
    mark_sidebar_dirty(meta.session_id)

  def _metadata_path(self, session_id: str, thread_id: str) -> Path:
    return self.thread_dir(session_id, thread_id) / "metadata.json"
