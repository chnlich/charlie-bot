"""Thread management for CharlieBot Worker tasks."""

import asyncio
import os
from datetime import datetime
from pathlib import Path

import aiofiles
import structlog

from src.core.config import CharlieBotConfig
from src.core.json_utils import write_model_json_atomically
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
    # metadata.json path -> (mtime_ns, size, parsed meta). Re-validating every
    # thread file on each 3 s workers-panel poll costs ~176 us per thread; a
    # rewrite always moves (mtime_ns, size), so the stale-key case cannot serve
    # old data. Entries for files missing from the latest scan are dropped, so
    # a deleted thread never lingers. Callers only read the returned metas:
    # the update path (update_status) re-reads through the uncached get_thread,
    # so no in-place mutation of a memoized instance exists to leak.
    self._list_memo: dict[str, tuple[int, int, ThreadMetadata]] = {}

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

    def load_all() -> list[ThreadMetadata]:
      # One executor hop for the whole scan: a per-file aiofiles read costs
      # ~0.5 ms in thread-pool hand-off, so per-file reads make the 3s
      # workers-panel poll scale linearly with thread count. The scan itself
      # runs on plain str paths: at this fan-out pathlib's join/str wrappers
      # cost more CPU than the stat syscalls they drive.
      if not threads_dir.is_dir():
        return []
      memo = self._list_memo
      refreshed: dict[str, tuple[int, int, ThreadMetadata]] = {}
      metas = []
      for entry in os.scandir(threads_dir):
        if not entry.is_dir():
          continue
        path = entry.path + "/metadata.json"
        try:
          st = os.stat(path)
        except OSError:
          # The pre-scandir gate was path.exists(), which also maps every
          # stat failure (absent file, permission) to "skip this thread".
          continue
        hit = memo.get(path)
        if hit is not None and hit[0] == st.st_mtime_ns and hit[1] == st.st_size:
          meta = hit[2]
        else:
          with open(path, encoding="utf-8") as f:
            meta = ThreadMetadata.model_validate_json(f.read())
        refreshed[path] = (st.st_mtime_ns, st.st_size, meta)
        metas.append(meta)
      # Swap whole dicts: concurrent load_all calls in the executor never mutate
      # the live map, and the swap keeps the memo down to threads still on disk.
      self._list_memo = refreshed
      return metas

    threads = await asyncio.to_thread(load_all)
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
    # list_threads reads this file from an executor thread with no
    # coordination, so the write must stay atomic (a validation failure 500s
    # the whole list endpoint).
    await write_model_json_atomically(self._metadata_path(meta.session_id, meta.id), meta)
    # Single funnel behind create_thread/update_status/save_metadata: thread
    # status transitions (running -> terminal) land here.
    mark_sidebar_dirty(meta.session_id)

  def _metadata_path(self, session_id: str, thread_id: str) -> Path:
    return self.thread_dir(session_id, thread_id) / "metadata.json"
