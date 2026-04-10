"""Thread management for CharlieBot Worker tasks."""

import asyncio
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

import aiofiles
import structlog

from src.core.config import CharlieBotConfig
from src.core.models import SessionMetadata, ThreadMetadata, ThreadStatus

log = structlog.get_logger()


class ThreadManager:
  """Creates and manages Worker threads."""

  def __init__(self, cfg: CharlieBotConfig):
    self._cfg = cfg
    self._thread_session_index: dict[str, str] = {}  # thread_id -> session_id

  async def create_thread(
      self,
      session_meta: SessionMetadata,
      description: str,
      branch_name: Optional[str] = None,
      review_of: Optional[str] = None,
      context: Optional[str] = None,
      require_review: bool = True,
  ) -> ThreadMetadata:
    """Create a new thread directory and metadata."""
    thread = ThreadMetadata(
        session_id=session_meta.id,
        description=description,
        branch_name=branch_name,
        review_of=review_of,
        context=context,
        require_review=require_review,
    )

    thread_dir = self._thread_dir(session_meta.id, thread.id)
    (thread_dir / "data").mkdir(parents=True, exist_ok=True)

    await self._save_metadata(thread)
    self._thread_session_index[thread.id] = session_meta.id
    log.info("thread_created", thread_id=thread.id)
    return thread

  async def get_thread(self, session_id: str, thread_id: str) -> Optional[ThreadMetadata]:
    path = self._metadata_path(session_id, thread_id)
    if not path.exists():
      return None
    async with aiofiles.open(path, "r") as f:
      raw = await f.read()
    self._thread_session_index[thread_id] = session_id
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
      pid: Optional[int] = None,
      exit_code: Optional[int] = None,
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
      meta.started_at = datetime.now(timezone.utc)
    if status in (ThreadStatus.COMPLETED, ThreadStatus.FAILED, ThreadStatus.CANCELLED):
      meta.completed_at = datetime.now(timezone.utc)
    await self._save_metadata(meta)

  async def get_events_log_path(self, session_id: str, thread_id: str) -> Path:
    return self._thread_dir(session_id, thread_id) / "data" / "events.jsonl"

  def resolve_events_file(self, thread_id: str) -> Path | None:
    """Resolve the events.jsonl path for a thread, using the in-memory index with directory-scan fallback."""
    # Fast path: check the index
    session_id = self._thread_session_index.get(thread_id)
    if session_id:
      candidate = self._cfg.sessions_dir / session_id / "threads" / thread_id / "data" / "events.jsonl"
      if candidate.exists():
        return candidate
      # Stale index entry — fall through to scan
      self._thread_session_index.pop(thread_id, None)

    # Fallback: O(n) directory scan
    if not self._cfg.sessions_dir.exists():
      return None
    for session_dir in self._cfg.sessions_dir.iterdir():
      if not session_dir.is_dir():
        continue
      candidate = session_dir / "threads" / thread_id / "data" / "events.jsonl"
      if candidate.exists():
        self._thread_session_index[thread_id] = session_dir.name
        return candidate
    return None

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

  def _thread_dir(self, session_id: str, thread_id: str) -> Path:
    return self._cfg.sessions_dir / session_id / "threads" / thread_id

  def _metadata_path(self, session_id: str, thread_id: str) -> Path:
    return self._thread_dir(session_id, thread_id) / "metadata.json"
