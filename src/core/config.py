"""Configuration loading for CharlieBot."""

import os
from pathlib import Path
from typing import Optional

import structlog
from pydantic import BaseModel, model_validator

log = structlog.get_logger()

from src.core.models import BackendOption
from src.core.yaml_utils import load_yaml


class ImprovementLoopConfig(BaseModel):
  """Declarative config for an improvement-loop cron task."""

  backlog: str  # relative path within repo, e.g. 'loop/backlog.yaml'
  role: str  # agent role description
  scope_files: list[str]  # files/dirs agent may modify
  id_prefix: str = ''  # e.g. 'D' for D-001, empty for plain 001
  language: str = 'en'  # 'en' or 'zh-CN'
  max_pending: int = 10
  stale_timeout_hours: float = 1.0
  state_files: list[str] = []  # extra files to read before acting
  verify: list[str] = []  # shell commands to run after implementing
  scan_prompt: str = ''  # module-specific instructions for health scan step
  idea_prompt: str = ''  # what to think about when generating new ideas
  extra_rules: list[str] = []  # module-specific rules appended to prompt


class ScheduledTaskConfig(BaseModel):
  """Configuration for a single scheduled (cron-like) task."""

  name: str
  cron: str
  prompt: Optional[str] = None
  handler: Optional[str] = None
  loop: Optional[ImprovementLoopConfig] = None
  repo: Optional[str] = None
  backend: Optional[str] = None
  timezone: str = "America/Los_Angeles"
  enabled: bool = True
  project: Optional[str] = None
  allow_failure: bool = False
  notify: Optional[str] = None  # 'telegram' or None

  @model_validator(mode='after')
  def check_prompt_or_handler_or_loop(self) -> 'ScheduledTaskConfig':
    sources = sum([bool(self.prompt), bool(self.handler), bool(self.loop)])
    if sources != 1:
      raise ValueError("task must have exactly one of 'prompt', 'handler', or 'loop'")
    if self.notify and self.notify != 'telegram':
      raise ValueError(f"notify must be 'telegram' or None, got '{self.notify}'")
    return self


class BacklogRepoConfig(BaseModel):
  """A single backlog repo entry: label + path."""
  label: str
  path: str


class CharlieBotConfig(BaseModel):
  """CharlieBot configuration, loaded from ~/.charliebot/config.yaml."""

  # LLM
  gemini_api_key: str = ""
  # Gemini API model for autonamer / audio transcription
  # (NOT the CLI backend model — that lives in backend_options[].model)
  gemini_model: str = "gemini-3.1-pro-preview"

  # Kimi (Moonshot) — optional, not wired in by default
  moonshot_api_key: Optional[str] = None

  # DeepSeek SGLang — OpenAI-compatible base URL, including /v1.
  # Used by CharlieBot's hosted Anthropic proxy.
  deepseek_sglang_base_url: Optional[str] = None
  deepseek_sglang_api_key: Optional[str] = None
  # Claude-facing Anthropic proxy base URL for DeepSeek SGLang, including
  # /api/anthropic-proxy/deepseek-sglang. If unset, use the local proxy URL.
  deepseek_sglang_proxy_base_url: Optional[str] = None

  # Authentication — shared secret; empty string disables auth
  charliebot_access_key: str = ""

  # Server
  server_host: str = "127.0.0.1"
  server_port: int = 8000

  # Paths
  charliebot_home: Path = Path.home() / ".charliebot"

  # Workspace directories to scan for git repos
  workspace_dirs: list[str] = ["~/workspace"]

  # Root directory for worker worktrees
  worktree_dir: str = "~/worktrees"

  # Backlog panel
  backlog_repos: list[BacklogRepoConfig] = []
  backlog_repo: Optional[str] = None  # deprecated, migrated to backlog_repos
  backlog_label: str = 'Project Backlog'  # deprecated, used during migration

  # SSL
  ssl_certfile: Optional[str] = None
  ssl_keyfile: Optional[str] = None

  # Subprocess stdout buffer limit in MB (for asyncio StreamReader)
  subprocess_buffer_limit_mb: int = 1024

  # Voice transcription: custom vocabulary hints for Gemini
  voice_custom_words: list[str] = []

  # Backend options available for model switching
  # Additional backends (Codex/Gemini/Kimi/Antigravity/etc.) must be configured via
  # ~/.charliebot/config.yaml -> backend_options.
  backend_options: list[BackendOption] = [
      BackendOption(
          id="claude-opus-4.8", label="CC \u00b7 Opus 4.8", type="cc-claude", model="claude-opus-4-8", effort="max"),
      BackendOption(
          id="claude-opus-4.8-fast",
          label="CC \u00b7 Opus 4.8 Fast",
          type="cc-claude",
          model="claude-opus-4-8",
          effort="medium",
          fast_mode=True),
      BackendOption(id="claude-tui", label="Claude TUI", type="tui-cli"),
  ]

  # Ordered preference list for reviewer backend selection.
  # Each entry is a BackendOption.id. The first entry that differs from
  # the worker's backend and resolves successfully is used for the reviewer.
  # Empty list (default) preserves current behavior: reviewer uses same backend as worker.
  model_preference: list[str] = []

  # Telegram notifications
  telegram_bot_token: Optional[str] = None
  telegram_chat_id: Optional[str] = None

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
    # Migrate old backlog_repo (singular) → backlog_repos list
    if values.get("backlog_repo") and not values.get("backlog_repos"):
      label = values.pop("backlog_label", "Backlog")
      repo = os.path.expanduser(values.pop("backlog_repo"))
      values["backlog_repos"] = [{"label": label, "path": repo}]
    elif values.get("backlog_repo"):
      values["backlog_repo"] = os.path.expanduser(values["backlog_repo"])
    # Expand ~ in backlog_repos entries
    for entry in values.get("backlog_repos", []):
      if isinstance(entry, dict) and entry.get("path"):
        entry["path"] = os.path.expanduser(entry["path"])
    return values

  @property
  def subprocess_buffer_limit(self) -> int:
    """Return the subprocess buffer limit in bytes."""
    return self.subprocess_buffer_limit_mb * 1024 * 1024

  @property
  def server_base_url(self) -> str:
    """Return the local base URL for CLI-to-server internal API calls."""
    scheme = "https" if self.ssl_certfile and self.ssl_keyfile else "http"
    return f"{scheme}://localhost:{self.server_port}"

  @property
  def deepseek_sglang_anthropic_proxy_base_url(self) -> str:
    """Return the Claude-facing Anthropic proxy base URL for DeepSeek SGLang."""
    if self.deepseek_sglang_proxy_base_url:
      return self.deepseek_sglang_proxy_base_url.rstrip("/")
    return f"{self.server_base_url}/api/anthropic-proxy/deepseek-sglang"

  @property
  def sessions_dir(self) -> Path:
    return self.charliebot_home / "sessions"

  @property
  def claude_md_file(self) -> Path:
    """The master agent prompt: ~/.charliebot/MASTER_AGENT_PROMPT.md."""
    return self.charliebot_home / "MASTER_AGENT_PROMPT.md"

  @property
  def memory_file(self) -> Path:
    return self.charliebot_home / "MEMORY.md"

  @property
  def memory_host_file(self) -> Path:
    return self.charliebot_home / "MEMORY.host.md"

  @property
  def charlie_bot_repo(self) -> Path:
    """Root of the charlie-bot repository (derived from package location)."""
    return Path(__file__).resolve().parents[2]

  @property
  def config_file(self) -> Path:
    return self.charliebot_home / "config.yaml"

  @property
  def config_d_dir(self) -> Path:
    return self.charliebot_home / "config.d"

  def get_backend_option(self, backend_id: str) -> Optional[BackendOption]:
    """Look up a backend option by id."""
    return next((opt for opt in self.backend_options if opt.id == backend_id), None)

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

  yaml_data: dict = load_yaml(config_path, default={})

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
  except FileNotFoundError:
    return _cron_tasks
  except OSError as e:
    log.warning("cron_config_stat_failed", error=str(e))
    return _cron_tasks
  if mtime != _cron_mtime:
    try:
      data = load_yaml(cron_path, default={})
      raw_tasks = data.get("scheduled_tasks", [])
      for t in raw_tasks:
        if isinstance(t, dict) and t.get("repo"):
          t["repo"] = os.path.expanduser(t["repo"])
      _cron_tasks = [ScheduledTaskConfig(**t) for t in raw_tasks]
      _cron_mtime = mtime
    except Exception as e:
      log.warning("cron_config_reload_failed", error=str(e))
  return _cron_tasks
