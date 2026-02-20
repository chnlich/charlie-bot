"""Thread management for CharlieBot Worker tasks."""

import time
from datetime import datetime
from pathlib import Path
from typing import Optional

import aiofiles
import structlog

from src.core.config import CharlieBotConfig
from src.core.git import GitError, GitManager
from src.core.models import SessionMetadata, Task, ThreadMetadata, ThreadStatus

log = structlog.get_logger()

_CLAUDE_DEFAULT_PATH = Path(__file__).parent.parent.parent / "config" / "claude-default.md"


class ThreadManager:
  """Creates and manages Worker threads (isolated git branches + worktrees)."""

  def __init__(self, cfg: CharlieBotConfig, git_manager: GitManager):
    self._cfg = cfg
    self._git = git_manager

  async def create_thread(
    self,
    session_meta: SessionMetadata,
    task: Task,
    is_conflict_resolver: bool = False,
  ) -> ThreadMetadata:
    """Create a new thread: git branch + worktree + CLAUDE.md."""
    ts = int(time.time())
    thread = ThreadMetadata(
      session_id=session_meta.id,
      task_id=task.id,
      description=task.description,
      branch_name="",  # Set below
      is_conflict_resolver=is_conflict_resolver,
    )

    prefix = "charliebot/conflict-" if is_conflict_resolver else "charliebot/task-"
    thread.branch_name = f"{prefix}{ts}-{thread.id[:8]}"

    thread_dir = self._thread_dir(session_meta.id, thread.id)
    (thread_dir / "data").mkdir(parents=True, exist_ok=True)

    # Write CLAUDE.md into the thread directory so Claude Code finds it
    await self._write_claude_md(thread_dir, task, session_meta, thread)

    await self._save_metadata(thread)
    log.info("thread_created", thread_id=thread.id, branch=thread.branch_name)
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

  async def cleanup_worktree(self, session_id: str, thread_id: str) -> None:
    """Remove the thread's git worktree (but keep metadata)."""
    meta = await self.get_thread(session_id, thread_id)
    if not meta or not meta.worktree_path:
      return
    worktree_path = Path(meta.worktree_path)
    if worktree_path.exists():
      try:
        # Use the worktree's parent repo to remove it
        await self._git.remove_worktree(worktree_path.parent, worktree_path)
      except GitError as e:
        log.warning("worktree_cleanup_failed", thread=thread_id, error=str(e))

  async def get_events_log_path(self, session_id: str, thread_id: str) -> Path:
    return self._thread_dir(session_id, thread_id) / "data" / "events.jsonl"

  async def get_worktree_path(self, session_id: str, thread_id: str) -> Path:
    meta = await self.get_thread(session_id, thread_id)
    if meta and meta.worktree_path:
      return Path(meta.worktree_path)
    return self._thread_dir(session_id, thread_id)

  async def get_claude_md_path(self, session_id: str, thread_id: str) -> Path:
    worktree = await self.get_worktree_path(session_id, thread_id)
    return worktree / "CLAUDE.md"

  # ---------------------------------------------------------------------------
  # Private helpers
  # ---------------------------------------------------------------------------

  async def _write_claude_md(
    self,
    thread_dir: Path,
    task: Task,
    session_meta: SessionMetadata,
    thread: ThreadMetadata,
  ) -> None:
    """Concatenate default instructions + task-specific content."""
    default_content = ""
    if _CLAUDE_DEFAULT_PATH.exists():
      default_content = _CLAUDE_DEFAULT_PATH.read_text(encoding="utf-8")

    plan_note = ""
    if task.is_plan_mode:
      plan_note = (
        "\n## Plan Mode\n"
        "This is a PLANNING task. Do NOT modify any files.\n"
        "Output a detailed, step-by-step execution plan as a numbered list.\n"
        "Each step should be independently actionable by a separate worker.\n\n"
      )

    context_note = ""
    if task.context:
      context_note = "\n## Additional Context\n" + "\n".join(f"- {k}: {v}" for k, v in task.context.items()) + "\n"

    content = (
      f"{default_content}\n"
      f"## Task Description\n\n{task.description}\n"
      f"{plan_note}"
      f"{context_note}"
      f"\n## Session Info\n"
      f"- Session: {session_meta.name}\n"
      f"- Branch: {task.id[:8]}\n"
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
