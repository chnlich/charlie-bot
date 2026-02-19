"""Configuration loading for CharlieBot."""

import os
from pathlib import Path
from typing import Optional

import yaml
from pydantic import BaseModel, field_validator


class CharliBotConfig(BaseModel):
  """CharlieBot configuration, loaded from ~/.charliebot/config.yaml."""

  # LLM
  gemini_api_key: str = ""
  gemini_model: str = "gemini-3-flash-preview"
  kimi_api_key: str = ""
  kimi_base_url: str = "https://api.moonshot.cn/v1"
  kimi_model: str = "kimi-k2.5"

  # Worker
  max_concurrent_workers: int = 3
  worker_timeout_seconds: int = 3600

  # Paths
  charliebot_home: Path = Path.home() / ".charliebot"

  # Workspace directories to scan for git projects
  project_dirs: list[str] = []

  @field_validator("project_dirs", mode="before")
  @classmethod
  def expand_project_dirs(cls, v: list[str]) -> list[str]:
    return [os.path.expanduser(p) for p in (v or [])]

  @property
  def sessions_dir(self) -> Path:
    return self.charliebot_home / "sessions"

  @property
  def logs_dir(self) -> Path:
    return self.charliebot_home / "logs"

  @property
  def memory_file(self) -> Path:
    return self.charliebot_home / "MEMORY.md"

  @property
  def progress_file(self) -> Path:
    return self.charliebot_home / "PROGRESS.md"

  @property
  def config_file(self) -> Path:
    return self.charliebot_home / "config.yaml"

  def discover_projects(self) -> list[dict[str, str]]:
    """Scan project_dirs for directories containing a .git folder."""
    projects: list[dict[str, str]] = []
    for dir_str in self.project_dirs:
      parent = Path(dir_str)
      if not parent.is_dir():
        continue
      for child in sorted(parent.iterdir()):
        if child.is_dir() and (child / ".git").exists():
          projects.append({"name": child.name, "path": str(child)})
    return projects


_config: Optional[CharliBotConfig] = None


def load_config() -> CharliBotConfig:
  """Load config from ~/.charliebot/config.yaml."""
  home = Path.home() / ".charliebot"
  config_path = home / "config.yaml"

  yaml_data: dict = {}
  if config_path.exists():
    with open(config_path) as f:
      yaml_data = yaml.safe_load(f) or {}

  yaml_data.setdefault("charliebot_home", str(home))
  return CharliBotConfig(**yaml_data)


def get_config() -> CharliBotConfig:
  """Return the singleton config instance."""
  global _config
  if _config is None:
    _config = load_config()
  return _config
