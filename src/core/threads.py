"""Thread management for CharlieBot Worker tasks."""

import time
from datetime import datetime
from pathlib import Path
from typing import Optional

import aiofiles
import structlog

from src.core.config import CharlieBotConfig
from src.core.models import SessionMetadata, ThreadMetadata, ThreadStatus

log = structlog.get_logger()

_CLAUDE_DEFAULT_PATH = Path(__file__).parent.parent.parent / "config" / "claude-default.md"


class ThreadManager:
  """Creates and manages Worker threads."""

  def __init__(self, cfg: CharlieBotConfig):
    self._cfg = cfg

  async def create_thread(
    self,
    session_meta: SessionMetadata,
    description: str,
  ) -> ThreadMetadata:
    """Create a new thread with a CLAUDE.md for the worker."""
    thread = ThreadMetadata(
      session_id=session_meta.id,
      description=description,
    )

    thread_dir = self._thread_dir(session_meta.id, thread.id)
    (thread_dir / "data").mkdir(parents=True, exist_ok=True)

    await self._write_claude_md(thread_dir, session_meta)

    await self._save_metadata(thread)
    log.info("thread_created", thread_id=thread.id)
    return thread

  async def get_thread(self, session_id: str, thread_id: str) -> Optional[ThreadMetadata]:
    path = self._metadata_path(session_id, thread_id)
    if not path.exists():
      return None
    async with aiofiles.open(path, "r") as f:
      raw = await f.read()
    return ThreadMetadata.model_validate_json(raw)

  async def list_threads(self, session_id: str) -> list[ThreadMetadata]:
    threads: list[ThreadMetadata] = []
    threads_dir = self._cfg.sessions_dir / session_id / "threads"
    if not threads_dir.exists():
      return threads
    for d in threads_dir.iterdir():
      if not d.is_dir():
        continue
      meta = await self.get_thread(session_id, d.name)
      if meta:
        threads.append(meta)
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
      meta.started_at = datetime.utcnow()
    if status in (ThreadStatus.COMPLETED, ThreadStatus.FAILED, ThreadStatus.CANCELLED):
      meta.completed_at = datetime.utcnow()
    await self._save_metadata(meta)

  async def get_events_log_path(self, session_id: str, thread_id: str) -> Path:
    return self._thread_dir(session_id, thread_id) / "data" / "events.jsonl"

  def get_thread_dir(self, session_id: str, thread_id: str) -> Path:
    return self._thread_dir(session_id, thread_id)

  # ---------------------------------------------------------------------------
  # Private helpers
  # ---------------------------------------------------------------------------

  async def _write_claude_md(
    self,
    thread_dir: Path,
    session_meta: SessionMetadata,
  ) -> None:
    """Write default instructions + session info (task is passed via -p)."""
    default_content = ""
    if _CLAUDE_DEFAULT_PATH.exists():
      default_content = _CLAUDE_DEFAULT_PATH.read_text(encoding="utf-8")

    content = (
      f"{default_content}\n"
      f"## Session Info\n"
      f"- Session: {session_meta.name}\n"
    )

    claude_md_path = thread_dir / "CLAUDE.md"
    async with aiofiles.open(claude_md_path, "w", encoding="utf-8") as f:
      await f.write(content)

  async def _save_metadata(self, meta: ThreadMetadata) -> None:
    path = self._metadata_path(meta.session_id, meta.id)
    path.parent.mkdir(parents=True, exist_ok=True)
    async with aiofiles.open(path, "w") as f:
      await f.write(meta.model_dump_json(indent=2))

  def _thread_dir(self, session_id: str, thread_id: str) -> Path:
    return self._cfg.sessions_dir / session_id / "threads" / thread_id

  def _metadata_path(self, session_id: str, thread_id: str) -> Path:
    return self._thread_dir(session_id, thread_id) / "metadata.json"
