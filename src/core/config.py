"""Configuration loading for CharlieBot."""

import os
from pathlib import Path
from typing import Optional

import yaml
from pydantic import BaseModel, field_validator, model_validator


class CharlieBotConfig(BaseModel):
  """CharlieBot configuration, loaded from ~/.charliebot/config.yaml."""

  # LLM
  gemini_api_key: str = ""
  gemini_model: str = "gemini-3.1-pro-preview"

  # Server
  server_port: int = 8000

  # Paths
  charliebot_home: Path = Path.home() / ".charliebot"

  # Workspace directories to scan for git repos
  workspace_dirs: list[str] = ["~/workspace"]

  # Root directory for worker worktrees
  worktree_dir: str = "~/worktrees"

  # Subprocess stdout buffer limit in MB (for asyncio StreamReader)
  subprocess_buffer_limit_mb: int = 1024

  @model_validator(mode="before")
  @classmethod
  def migrate_and_expand(cls, values: dict) -> dict:
    """Backward compat: rename project_dirs -> workspace_dirs, expand ~ in paths."""
    if "project_dirs" in values and "workspace_dirs" not in values:
      values["workspace_dirs"] = values.pop("project_dirs")
    elif "project_dirs" in values:
      values.pop("project_dirs")
    # Remove deprecated fields silently
    values.pop("max_concurrent_workers", None)
    # Expand ~ in workspace_dirs and worktree_dir
    ws = values.get("workspace_dirs", ["~/workspace"])
    values["workspace_dirs"] = [os.path.expanduser(p) for p in ws]
    wd = values.get("worktree_dir", "~/worktrees")
    values["worktree_dir"] = os.path.expanduser(wd)
    return values

  @field_validator("workspace_dirs", mode="before")
  @classmethod
  def expand_workspace_dirs(cls, v: list[str]) -> list[str]:
    return [os.path.expanduser(p) for p in (v or [])]

  @field_validator("worktree_dir", mode="before")
  @classmethod
  def expand_worktree_dir(cls, v: str) -> str:
    return os.path.expanduser(v)

  @property
  def subprocess_buffer_limit(self) -> int:
    """Return the subprocess buffer limit in bytes."""
    return self.subprocess_buffer_limit_mb * 1024 * 1024

  @property
  def sessions_dir(self) -> Path:
    return self.charliebot_home / "sessions"

  @property
  def claude_md_file(self) -> Path:
    """The master agent prompt: ~/.charliebot/MASTER_AGENT_PROMPT.md."""
    return self.charliebot_home / "MASTER_AGENT_PROMPT.md"

  def session_claude_md(self, session_id: str) -> Path:
    """Per-session CLAUDE.md: concatenation of MASTER_AGENT_PROMPT.md + MEMORY.md."""
    return self.sessions_dir / session_id / "CLAUDE.md"

  @property
  def memory_file(self) -> Path:
    return self.charliebot_home / "MEMORY.md"

  @property
  def config_file(self) -> Path:
    return self.charliebot_home / "config.yaml"

  def discover_repos(self) -> list[dict[str, str]]:
    """Scan workspace_dirs for directories containing a .git folder."""
    repos: list[dict[str, str]] = []
    for dir_str in self.workspace_dirs:
      parent = Path(dir_str)
      if not parent.is_dir():
        continue
      for child in sorted(parent.iterdir()):
        if child.is_dir() and (child / ".git").exists():
          repos.append({"name": child.name, "path": str(child)})
    return repos


_config: Optional[CharlieBotConfig] = None


def load_config() -> CharlieBotConfig:
  """Load config from ~/.charliebot/config.yaml."""
  home = Path.home() / ".charliebot"
  config_path = home / "config.yaml"

  yaml_data: dict = {}
  if config_path.exists():
    with open(config_path) as f:
      yaml_data = yaml.safe_load(f) or {}

  yaml_data.setdefault("charliebot_home", str(home))
  return CharlieBotConfig(**yaml_data)


def get_config() -> CharlieBotConfig:
  """Return the singleton config instance."""
  global _config
  if _config is None:
    _config = load_config()
  return _config
