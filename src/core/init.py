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
  recovered = 0
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
      recovered += 1
      log.warning("recovered_orphaned_thread", thread=meta.get("id"), pid=pid)
  if recovered:
    log.info("orphaned_thread_recovery_done", count=recovered)
