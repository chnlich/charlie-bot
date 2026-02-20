"""Direct worker spawner — replaces the queue+dispatcher with a single function."""

import json
import traceback
from datetime import datetime

import structlog

from src.agents.worker import WORKER_COMMAND, QuotaExhaustedException, Worker
from src.core.models import ThreadStatus
from src.core.sessions import SessionManager
from src.core.streaming import streaming_manager
from src.core.threads import ThreadManager
from src.core.config import CharlieBotConfig

log = structlog.get_logger()


async def spawn_worker(
  session_id: str,
  description: str,
  thread_id: str,
  cfg: CharlieBotConfig,
  session_mgr: SessionManager,
  thread_mgr: ThreadManager,
) -> None:
  """Spawn a Claude Code worker for the given thread. Fire-and-forget via asyncio.create_task()."""
  try:
    thread = await thread_mgr.get_thread(session_id, thread_id)
    if not thread:
      log.error("spawn_worker_thread_missing", session=session_id, thread_id=thread_id)
      return

    # Determine working directory: first discovered repo, or thread dir
    repos = cfg.discover_repos()
    working_dir = thread_mgr.get_thread_dir(session_id, thread_id)
    if repos:
      from pathlib import Path
      working_dir = Path(repos[0]["path"])

    # Build and run Worker
    events_log = await thread_mgr.get_events_log_path(session_id, thread_id)
    worker = Worker(
      thread, working_dir, events_log, description,
      on_spawned=thread_mgr._save_metadata,
    )

    # Mark RUNNING
    thread.cli_command = " ".join(WORKER_COMMAND + [description])
    thread.status = ThreadStatus.RUNNING
    thread.started_at = datetime.utcnow()
    await thread_mgr._save_metadata(thread)
    log.info("worker_running", thread_id=thread.id, session=session_id)

    try:
      exit_code = await worker.run()
      if exit_code == 0:
        await thread_mgr.update_status(session_id, thread.id, ThreadStatus.COMPLETED, exit_code=0)
        log.info("worker_completed", thread_id=thread.id)
      else:
        await thread_mgr.update_status(session_id, thread.id, ThreadStatus.FAILED, exit_code=exit_code)
        log.warning("worker_failed_nonzero", thread_id=thread.id, exit_code=exit_code)

      await _notify_completion(session_id, description, thread, exit_code, thread_mgr, session_mgr)

    except QuotaExhaustedException:
      await worker.terminate()
      await thread_mgr.update_status(session_id, thread.id, ThreadStatus.FAILED)
      log.warning("worker_quota_exhausted", thread_id=thread.id)
      await _notify_completion(session_id, description, thread, -1, thread_mgr, session_mgr, quota_exhausted=True)

    except Exception as e:
      await worker.terminate()
      await thread_mgr.update_status(session_id, thread.id, ThreadStatus.FAILED, exit_code=-1)
      log.error("worker_failed", thread_id=thread.id, error=str(e), traceback=traceback.format_exc())
      await _notify_completion(session_id, description, thread, -1, thread_mgr, session_mgr, error=str(e))

  except Exception as e:
    log.error("spawn_worker_setup_failed", session=session_id, error=str(e), traceback=traceback.format_exc())


async def _notify_completion(
  session_id: str,
  description: str,
  thread,
  exit_code: int,
  thread_mgr: ThreadManager,
  session_mgr: SessionManager,
  quota_exhausted: bool = False,
  error: str = "",
) -> None:
  """Broadcast worker_summary event to the session WebSocket."""
  try:
    events_summary = await _read_events_summary(session_id, thread.id, thread_mgr)

    status = "completed" if exit_code == 0 else "failed"
    summary = f"**Worker finished: {description}**\n\n{events_summary}"
    if quota_exhausted:
      summary += "\n\n*Worker stopped: API quota exhausted.*"
    elif error:
      summary += f"\n\n*Worker error: {error}*"
    elif exit_code != 0:
      summary += f"\n\n*Worker exited with code {exit_code}.*"

    await session_mgr.mark_unread(session_id)
    await streaming_manager.broadcast(f"session:{session_id}", {
      "type": "worker_summary",
      "thread_id": thread.id,
      "content": summary,
      "status": status,
    })
    log.info("worker_summary_sent", session=session_id, thread=thread.id)

  except Exception as e:
    log.error("notify_completion_failed", thread_id=thread.id, error=str(e))
    try:
      fallback = f"Worker finished task: {description}\n\n(Unable to generate summary: {e})"
      await session_mgr.mark_unread(session_id)
      await streaming_manager.broadcast(f"session:{session_id}", {
        "type": "worker_summary",
        "thread_id": thread.id,
        "content": fallback,
        "status": "completed" if exit_code == 0 else "failed",
      })
    except Exception as inner:
      log.error("fallback_notify_failed", thread_id=thread.id, error=str(inner))


async def _read_events_summary(session_id: str, thread_id: str, thread_mgr: ThreadManager, max_lines: int = 80) -> str:
  """Read the last N lines from a thread's events.jsonl for summarization."""
  events_path = await thread_mgr.get_events_log_path(session_id, thread_id)
  if not events_path.exists():
    return "(no events recorded)"
  lines = events_path.read_text(encoding="utf-8").strip().splitlines()
  tail = lines[-max_lines:] if len(lines) > max_lines else lines
  parts = []
  for line in tail:
    try:
      ev = json.loads(line)
      ev_type = ev.get("type", "unknown")
      content = _extract_event_content(ev, ev_type)
      if content:
        parts.append(f"[{ev_type}] {content}")
    except json.JSONDecodeError as e:
      log.debug("event_line_not_json", error=str(e))
  return "\n".join(parts) if parts else "(empty event log)"


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
