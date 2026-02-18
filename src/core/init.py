"""Initialize ~/.charliebot/ directory structure on first run."""

import os
import yaml
from pathlib import Path

from src.core.config import CharliBotConfig, get_config


def _default_config_yaml() -> dict:
  """Build the default config dict, seeding API keys from env vars if available."""
  return {
    "gemini_api_key": os.environ.get("GEMINI_API_KEY", ""),
    "gemini_model": "gemini-3-flash-preview",
    "kimi_api_key": os.environ.get("KIMI_API_KEY", ""),
    "kimi_base_url": "https://api.moonshot.cn/v1",
    "kimi_model": "kimi-k2.5",
    "max_concurrent_workers": 3,
    "worker_timeout_seconds": 3600,
  }

DEFAULT_MEMORY = "# MEMORY\n\nUser preferences, facts, and personalization notes are recorded here.\n"
DEFAULT_PAST_TASKS = "# PAST TASKS\n\nArchive of all completed tasks. Entries separated by ---\n"
DEFAULT_PROGRESS = "# PROGRESS\n\nLessons learned, best practices, and insights discovered during tasks.\n"


async def init_charliebot_home() -> None:
  """Ensure ~/.charliebot/ directory structure exists and seed default files."""
  cfg = get_config()

  # Create all required directories
  dirs = [
    cfg.charliebot_home,
    cfg.sessions_dir,
    cfg.backups_dir,
    cfg.logs_dir,
    cfg.repos_dir,
  ]
  for d in dirs:
    d.mkdir(parents=True, exist_ok=True)

  # Seed global knowledge files
  _seed_if_missing(cfg.memory_file, DEFAULT_MEMORY)
  _seed_if_missing(cfg.past_tasks_file, DEFAULT_PAST_TASKS)
  _seed_if_missing(cfg.progress_file, DEFAULT_PROGRESS)

  # Seed config.yaml with placeholders if missing
  if not cfg.config_file.exists():
    with open(cfg.config_file, "w") as f:
      yaml.dump(_default_config_yaml(), f, default_flow_style=False, sort_keys=False)


def _seed_if_missing(path: Path, content: str) -> None:
  """Write content to path only if the file does not already exist."""
  if not path.exists():
    path.write_text(content, encoding="utf-8")
