"""Configuration loading for CharlieBot."""

import os
from pathlib import Path
from typing import Optional

import yaml
import structlog
from pydantic import BaseModel, field_validator, model_validator

log = structlog.get_logger()

from src.core.models import BackendOption


class ScheduledTaskConfig(BaseModel):
  """Configuration for a single scheduled (cron-like) task."""

  name: str
  cron: str
  prompt: str
  repo: Optional[str] = None
  timezone: str = "America/New_York"
  enabled: bool = True


class CharlieBotConfig(BaseModel):
  """CharlieBot configuration, loaded from ~/.charliebot/config.yaml."""

  # LLM
  gemini_api_key: str = ""
  gemini_model: str = "gemini-3.1-pro-preview"

  # Kimi (Moonshot) — optional, not wired in by default
  moonshot_api_key: Optional[str] = None
  kimi_model: str = "kimi-k2.5"

  # Server
  server_port: int = 8000

  # Paths
  charliebot_home: Path = Path.home() / ".charliebot"

  # Workspace directories to scan for git repos
  workspace_dirs: list[str] = ["~/workspace"]

  # Root directory for worker worktrees
  worktree_dir: str = "~/worktrees"

  # SSL
  ssl_certfile: Optional[str] = None
  ssl_keyfile: Optional[str] = None

  # Subprocess stdout buffer limit in MB (for asyncio StreamReader)
  subprocess_buffer_limit_mb: int = 1024

  # Voice transcription: custom vocabulary hints for Gemini
  voice_custom_words: list[str] = []

  # Backend options available for model switching
  backend_options: list[BackendOption] = [
      BackendOption(id="claude-opus-4.6", label="CC \u00b7 Opus 4.6", type="cc-claude", model="claude-opus-4-6"),
      BackendOption(id="kimi-k2.5", label="CC \u00b7 Kimi K2.5", type="cc-kimi", model="kimi-k2.5"),
  ]

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
    if values.get("ssl_certfile"):
      values["ssl_certfile"] = os.path.expanduser(values["ssl_certfile"])
    if values.get("ssl_keyfile"):
      values["ssl_keyfile"] = os.path.expanduser(values["ssl_keyfile"])
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
  def subagent_prompt_file(self) -> Path:
    """The subagent (worker) prompt template: ~/.charliebot/SUBAGENT_PROMPT.md."""
    return self.charliebot_home / "SUBAGENT_PROMPT.md"

  @property
  def memory_file(self) -> Path:
    return self.charliebot_home / "MEMORY.md"

  @property
  def config_file(self) -> Path:
    return self.charliebot_home / "config.yaml"

  @property
  def config_d_dir(self) -> Path:
    return self.charliebot_home / "config.d"

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
_config_mtime: float = 0.0


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
  """Return cached config, auto-reloading when config.yaml changes."""
  global _config, _config_mtime
  config_path = Path.home() / ".charliebot" / "config.yaml"
  try:
    mtime = config_path.stat().st_mtime
  except OSError:
    mtime = 0.0
  if _config is None or mtime != _config_mtime:
    try:
      _config = load_config()
      _config_mtime = mtime
    except Exception as e:
      log.warning("config_reload_failed", error=str(e))
      if _config is None:
        raise
  return _config


_cron_tasks: list[ScheduledTaskConfig] = []
_cron_mtime: float = 0.0


def get_scheduled_tasks() -> list[ScheduledTaskConfig]:
  """Load scheduled tasks from config.d/cron.yaml, with mtime cache."""
  global _cron_tasks, _cron_mtime
  cron_path = Path.home() / ".charliebot" / "config.d" / "cron.yaml"
  try:
    mtime = cron_path.stat().st_mtime
  except OSError:
    return _cron_tasks
  if mtime != _cron_mtime:
    try:
      data = yaml.safe_load(cron_path.read_text()) or {}
      raw_tasks = data.get("scheduled_tasks", [])
      for t in raw_tasks:
        if isinstance(t, dict) and t.get("repo"):
          t["repo"] = os.path.expanduser(t["repo"])
      _cron_tasks = [ScheduledTaskConfig(**t) for t in raw_tasks]
      _cron_mtime = mtime
    except Exception as e:
      log.warning("cron_config_reload_failed", error=str(e))
  return _cron_tasks
