"""Initialize ~/.charliebot/ directory structure on first run."""

import json
import os
from datetime import datetime

import yaml
from pathlib import Path

import structlog

from src.core.config import get_config

log = structlog.get_logger()


def _default_config_yaml() -> dict:
  """Build the default config dict with placeholder values."""
  return {
    "gemini_api_key": "",
    "gemini_model": "gemini-3-flash-preview",
    "kimi_api_key": "",
    "kimi_base_url": "https://api.moonshot.cn/v1",
    "kimi_model": "kimi-k2.5",
    "max_concurrent_workers": 3,
    "worker_timeout_seconds": 3600,
    "workspace_dirs": ["~/workspace"],
    "worktree_dir": "~/worktrees",
  }

DEFAULT_MEMORY = "# MEMORY\n\nUser preferences, facts, and personalization notes are recorded here.\n"
DEFAULT_PROGRESS = "# PROGRESS\n\nLessons learned, best practices, and insights discovered during tasks.\n"


async def init_charliebot_home() -> None:
  """Ensure ~/.charliebot/ directory structure exists and seed default files."""
  cfg = get_config()

  # Create all required directories
  dirs = [
    cfg.charliebot_home,
    cfg.sessions_dir,
    cfg.logs_dir,
  ]
  for d in dirs:
    d.mkdir(parents=True, exist_ok=True)

  # Seed global knowledge files
  _seed_if_missing(cfg.memory_file, DEFAULT_MEMORY)
  _seed_if_missing(cfg.progress_file, DEFAULT_PROGRESS)

  # Seed config.yaml with placeholders if missing
  if not cfg.config_file.exists():
    with open(cfg.config_file, "w") as f:
      yaml.dump(_default_config_yaml(), f, default_flow_style=False, sort_keys=False)

  # Recover orphaned threads from previous server crash/restart
  _recover_orphaned_threads(cfg)


def _seed_if_missing(path: Path, content: str) -> None:
  """Write content to path only if the file does not already exist."""
  if not path.exists():
    path.write_text(content, encoding="utf-8")


def _recover_orphaned_threads(cfg) -> None:
  """Mark all threads stuck in 'running' as 'failed'.

  On server startup, no thread should be running — they are always spawned
  by the dispatcher. Any 'running' thread is orphaned from a previous server
  lifecycle (crash, reload, or restart). Kill any lingering worker processes.
  """
  if not cfg.sessions_dir.exists():
    return
  # Collect orphans per session so we can update conversation history
  orphans_by_session: dict[str, list[dict]] = {}
  for session_dir in cfg.sessions_dir.iterdir():
    threads_dir = session_dir / "threads"
    if not threads_dir.is_dir():
      continue
    for thread_dir in threads_dir.iterdir():
      meta_path = thread_dir / "metadata.json"
      if not meta_path.exists():
        continue
      try:
        meta = json.loads(meta_path.read_text(encoding="utf-8"))
      except (json.JSONDecodeError, OSError):
        continue
      if meta.get("status") != "running":
        continue
      # Kill orphaned worker process if still alive
      pid = meta.get("pid")
      if pid:
        try:
          os.kill(pid, 15)  # SIGTERM
        except (OSError, ProcessLookupError):
          pass
      # Mark as failed
      meta["status"] = "failed"
      meta["exit_code"] = -1
      meta["completed_at"] = datetime.utcnow().isoformat()
      meta_path.write_text(json.dumps(meta, indent=2), encoding="utf-8")
      log.warning("recovered_orphaned_thread", thread=meta.get("id"), pid=pid)
      session_id = meta.get("session_id", session_dir.name)
      orphans_by_session.setdefault(session_id, []).append(meta)

  # Append failure messages to conversation history and fix task queues
  for session_id, orphans in orphans_by_session.items():
    _append_recovery_messages(cfg, session_id, orphans)
    _mark_tasks_failed(cfg, session_id, orphans)

  total = sum(len(v) for v in orphans_by_session.values())
  if total:
    log.info("orphaned_thread_recovery_done", count=total)


def _append_recovery_messages(cfg, session_id: str, orphans: list[dict]) -> None:
  """Append failure messages to the session's conversation so the user sees feedback."""
  conv_path = cfg.sessions_dir / session_id / "data" / "conversation.json"
  if not conv_path.exists():
    return
  try:
    conv = json.loads(conv_path.read_text(encoding="utf-8"))
  except (json.JSONDecodeError, OSError):
    return
  import uuid
  for meta in orphans:
    desc = meta.get("description", "unknown task")
    msg = {
      "id": str(uuid.uuid4()),
      "role": "assistant",
      "content": f"Worker task was interrupted by a server restart: **{desc}**\n\nThe task did not complete. You can re-send the request to try again.",
      "timestamp": datetime.utcnow().isoformat(),
      "is_voice": False,
      "thread_id": meta.get("id"),
    }
    conv.setdefault("messages", []).append(msg)
  conv_path.write_text(json.dumps(conv, indent=2), encoding="utf-8")


def _mark_tasks_failed(cfg, session_id: str, orphans: list[dict]) -> None:
  """Mark corresponding tasks in the queue as failed."""
  queue_path = cfg.sessions_dir / session_id / "task_queue.json"
  if not queue_path.exists():
    return
  try:
    queue = json.loads(queue_path.read_text(encoding="utf-8"))
  except (json.JSONDecodeError, OSError):
    return
  orphan_task_ids = {m.get("task_id") for m in orphans}
  for task in queue.get("tasks", []):
    if task.get("id") in orphan_task_ids and task.get("status") == "running":
      task["status"] = "failed"
  queue["updated_at"] = datetime.utcnow().isoformat()
  queue_path.write_text(json.dumps(queue, indent=2), encoding="utf-8")
