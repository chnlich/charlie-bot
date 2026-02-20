"""Per-session queue dispatcher: pops tasks, creates threads, and spawns workers."""

from __future__ import annotations

import asyncio
import json
import traceback
from datetime import datetime
from typing import Optional

import structlog

from src.agents.worker import WORKER_COMMAND, QuotaExhaustedException, Worker
from src.core.models import ChatMessage, MessageRole, Task, ThreadStatus
from src.core.queue import QueueManager
from src.core.sessions import SessionManager
from src.core.streaming import streaming_manager
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

      # Build and run Worker
      events_log = await self._thread_mgr.get_events_log_path(self._session_id, thread.id)
      worktree = await self._thread_mgr.get_worktree_path(self._session_id, thread.id)
      worker = Worker(
        thread, worktree, events_log, task.description,
        on_spawned=self._thread_mgr._save_metadata,
      )

      # Store debug metadata and mark as RUNNING in a single save
      thread.cli_command = " ".join(WORKER_COMMAND + [task.description])
      thread.worktree_path = str(worktree)
      thread.status = ThreadStatus.RUNNING
      thread.started_at = datetime.utcnow()
      await self._thread_mgr._save_metadata(thread)
      log.info("thread_running", thread_id=thread.id, task_id=task.id)

      try:
        exit_code = await worker.run()
        if exit_code == 0:
          await self._queue_mgr.mark_complete(task.id)
          await self._thread_mgr.update_status(
            self._session_id, thread.id, ThreadStatus.COMPLETED, exit_code=0
          )
          log.info("task_completed", task_id=task.id, thread_id=thread.id)
        else:
          await self._queue_mgr.mark_failed(task.id)
          await self._thread_mgr.update_status(
            self._session_id, thread.id, ThreadStatus.FAILED, exit_code=exit_code
          )
          log.warning("task_failed_nonzero", task_id=task.id, exit_code=exit_code)

        # Broadcast completion directly (no LLM summarization)
        await self._notify_completion(task, thread, exit_code)

      except QuotaExhaustedException:
        await worker.terminate()
        await self._queue_mgr.mark_pending_quota(task.id)
        await self._thread_mgr.update_status(self._session_id, thread.id, ThreadStatus.FAILED)
        log.warning("task_quota_exhausted", task_id=task.id)
        await self._notify_completion(task, thread, -1, quota_exhausted=True)

      except asyncio.CancelledError:
        await worker.terminate()
        await self._queue_mgr.mark_failed(task.id)
        await self._thread_mgr.update_status(
          self._session_id, thread.id, ThreadStatus.CANCELLED, exit_code=-1
        )
        log.warning("task_cancelled", task_id=task.id, thread_id=thread.id)
        raise  # Re-raise so the asyncio task is properly cancelled

      except Exception as e:
        await worker.terminate()
        await self._queue_mgr.mark_failed(task.id)
        await self._thread_mgr.update_status(
          self._session_id, thread.id, ThreadStatus.FAILED, exit_code=-1
        )
        log.error(
          "task_failed", task_id=task.id,
          error=str(e), traceback=traceback.format_exc(),
        )
        await self._notify_completion(task, thread, -1, error=str(e))

    except Exception as e:
      # Catch-all for errors before the inner try (create_thread, get_worktree, etc.)
      log.error(
        "run_task_setup_failed", task_id=task.id,
        error=str(e), traceback=traceback.format_exc(),
      )
      await self._queue_mgr.mark_failed(task.id)

    finally:
      self._semaphore.release()

  async def _notify_completion(
    self,
    task: Task,
    thread,
    exit_code: int,
    quota_exhausted: bool = False,
    error: str = "",
  ) -> None:
    """Broadcast worker_summary event directly (no LLM summarization)."""
    try:
      events_summary = await self._read_events_summary(thread.id)

      status = "completed" if exit_code == 0 else "failed"
      summary = f"**Worker finished: {task.description}**\n\n{events_summary}"
      if quota_exhausted:
        summary += "\n\n*Worker stopped: API quota exhausted.*"
      elif error:
        summary += f"\n\n*Worker error: {error}*"
      elif exit_code != 0:
        summary += f"\n\n*Worker exited with code {exit_code}.*"

      # Mark session as having unread activity
      await self._session_mgr.mark_unread(self._session_id)

      # Broadcast to session WebSocket subscribers
      await streaming_manager.broadcast(f"session:{self._session_id}", {
        "type": "worker_summary",
        "thread_id": thread.id,
        "task_id": task.id,
        "content": summary,
        "status": status,
      })
      log.info("worker_summary_sent", session=self._session_id, thread=thread.id)

    except Exception as e:
      log.error("notify_completion_failed", task_id=task.id, error=str(e))

      # Still push a fallback message
      try:
        fallback = f"Worker finished task: {task.description}\n\n(Unable to generate summary: {e})"
        await self._session_mgr.mark_unread(self._session_id)
        await streaming_manager.broadcast(f"session:{self._session_id}", {
          "type": "worker_summary",
          "thread_id": thread.id,
          "task_id": task.id,
          "content": fallback,
          "status": "completed" if exit_code == 0 else "failed",
        })
      except Exception as inner:
        log.error("fallback_notify_failed", task_id=task.id, error=str(inner))

  async def _read_events_summary(self, thread_id: str, max_lines: int = 80) -> str:
    """Read the last N lines from a thread's events.jsonl for summarization."""
    events_path = await self._thread_mgr.get_events_log_path(self._session_id, thread_id)
    if not events_path.exists():
      return "(no events recorded)"
    lines = events_path.read_text(encoding="utf-8").strip().splitlines()
    tail = lines[-max_lines:] if len(lines) > max_lines else lines
    parts = []
    for line in tail:
      try:
        ev = json.loads(line)
        ev_type = ev.get("type", "unknown")
        content = self._extract_event_content(ev, ev_type)
        if content:
          parts.append(f"[{ev_type}] {content}")
      except json.JSONDecodeError as e:
        log.debug("event_line_not_json", error=str(e))
    return "\n".join(parts) if parts else "(empty event log)"

  @staticmethod
  def _extract_event_content(ev: dict, ev_type: str) -> str:
    """Extract human-readable content from a Claude Code stream-json event."""
    if ev_type == "result":
      return str(ev.get("result", ""))[:500]

    if ev_type == "assistant":
      msg = ev.get("message", {})
      blocks = msg.get("content", []) if isinstance(msg, dict) else []
      texts = []
      for block in blocks if isinstance(blocks, list) else []:
        if isinstance(block, dict):
          if block.get("type") == "text":
            texts.append(block.get("text", ""))
          elif block.get("type") == "tool_use":
            texts.append(f"[tool_use: {block.get('name', '?')}]")
      return " ".join(texts)[:300] if texts else ""

    if ev_type in ("thinking", "error", "complete", "tool_result", "tool_use", "file_write"):
      content = ev.get("content", ev.get("message", ""))
      if isinstance(content, list):
        texts = [b.get("text", "") for b in content if isinstance(b, dict) and b.get("type") == "text"]
        return " ".join(texts)[:200] if texts else ""
      return str(content)[:200]

    return ""


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
    _dispatchers[session_id] = SessionDispatcher(
      session_id, cfg, session_mgr, thread_mgr,
    )
  return _dispatchers[session_id]
