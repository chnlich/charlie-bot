"""Per-session queue dispatcher: pops tasks, creates threads, and spawns workers."""

from __future__ import annotations

import asyncio
import json
from typing import TYPE_CHECKING, Optional

import structlog

from src.agents.worker import WORKER_COMMAND, QuotaExhaustedException, Worker
from src.core.models import ChatMessage, MessageRole, Task, ThreadStatus
from src.core.queue import QueueManager
from src.core.sessions import SessionManager
from src.core.streaming import streaming_manager
from src.core.threads import ThreadManager
from src.core.config import CharliBotConfig

if TYPE_CHECKING:
  from src.agents.master_agent import MasterAgent

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
    master_agent: MasterAgent,
  ):
    self._session_id = session_id
    self._queue_mgr = QueueManager(session_id, cfg)
    self._session_mgr = session_mgr
    self._thread_mgr = thread_mgr
    self._master_agent = master_agent
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

      await self._thread_mgr.update_status(self._session_id, thread.id, ThreadStatus.RUNNING)
      log.info("thread_running", thread_id=thread.id, task_id=task.id)

      # Build and run Worker
      events_log = await self._thread_mgr.get_events_log_path(self._session_id, thread.id)
      worktree = await self._thread_mgr.get_worktree_path(self._session_id, thread.id)
      worker = Worker(thread, worktree, events_log, task.description)

      try:
        # Store CLI command and worktree path for debug mode
        cli_str = " ".join(WORKER_COMMAND + [task.description])
        thread.cli_command = cli_str
        thread.worktree_path = str(worktree)
        await self._thread_mgr._save_metadata(thread)

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

        # Ask master to review and summarize the worker's output
        await self._notify_completion(task, thread, exit_code)

      except QuotaExhaustedException:
        await self._queue_mgr.mark_pending_quota(task.id)
        await self._thread_mgr.update_status(self._session_id, thread.id, ThreadStatus.FAILED)
        log.warning("task_quota_exhausted", task_id=task.id)
        await self._notify_completion(task, thread, -1, quota_exhausted=True)

      except Exception as e:
        await self._queue_mgr.mark_failed(task.id)
        await self._thread_mgr.update_status(self._session_id, thread.id, ThreadStatus.FAILED)
        log.error("task_failed", task_id=task.id, error=str(e))
        await self._notify_completion(task, thread, -1, error=str(e))

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
    """Ask Master Agent to summarize the result and push it to chat + WebSocket."""
    try:
      # Build a short events summary from the on-disk log
      events_summary = await self._read_events_summary(thread.id)
      if quota_exhausted:
        events_summary += "\n[Worker stopped: API quota exhausted]"
      elif error:
        events_summary += f"\n[Worker error: {error}]"
      elif exit_code != 0:
        events_summary += f"\n[Worker exited with code {exit_code}]"

      # Ask master to review
      summary = await self._master_agent.review_worker_result(task.description, events_summary)

      # Append to session conversation history
      history = await self._session_mgr.load_history(self._session_id)
      assistant_msg = ChatMessage(
        role=MessageRole.ASSISTANT,
        content=summary,
        thread_id=thread.id,
      )
      history.messages.append(assistant_msg)
      await self._session_mgr.save_history(history)

      # Broadcast to session WebSocket subscribers so the frontend picks it up
      await streaming_manager.broadcast(f"session:{self._session_id}", {
        "type": "worker_summary",
        "thread_id": thread.id,
        "task_id": task.id,
        "content": summary,
        "status": "completed" if exit_code == 0 else "failed",
      })
      log.info("worker_summary_sent", session=self._session_id, thread=thread.id)

    except Exception as e:
      log.error("notify_completion_failed", task_id=task.id, error=str(e))

      # Still push a fallback message so the user isn't left with no reply
      try:
        fallback = f"Worker finished task: {task.description}\n\n(Unable to generate summary: {e})"
        history = await self._session_mgr.load_history(self._session_id)
        assistant_msg = ChatMessage(
          role=MessageRole.ASSISTANT,
          content=fallback,
          thread_id=thread.id,
        )
        history.messages.append(assistant_msg)
        await self._session_mgr.save_history(history)

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
      except json.JSONDecodeError:
        pass
    return "\n".join(parts) if parts else "(empty event log)"

  @staticmethod
  def _extract_event_content(ev: dict, ev_type: str) -> str:
    """Extract human-readable content from a Claude Code stream-json event."""
    if ev_type == "result":
      # Final output — the most important event
      return str(ev.get("result", ""))[:500]

    if ev_type == "assistant":
      # Content blocks nested in message.content[].text
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
      # tool_result content can be a list of blocks
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
  master_agent: MasterAgent,
) -> SessionDispatcher:
  if session_id not in _dispatchers:
    _dispatchers[session_id] = SessionDispatcher(
      session_id, cfg, session_mgr, thread_mgr, master_agent
    )
  return _dispatchers[session_id]
