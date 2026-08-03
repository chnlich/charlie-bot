"""Configuration loading for CharlieBot."""

import os
from pathlib import Path
from typing import Optional
from zoneinfo import ZoneInfo

import structlog
from pydantic import BaseModel, Field, model_validator

from src.core.models import BackendOption
from src.core.yaml_utils import load_yaml

log = structlog.get_logger()

CHARLIEBOT_HOME_ENV = "CHARLIEBOT_HOME"


def default_charliebot_home() -> Path:
  """The state directory used when ``CHARLIEBOT_HOME`` is unset."""
  return Path.home() / ".charliebot"


def charliebot_home_dir() -> Path:
  """Return the state directory this process belongs to (its profile).

  ``CHARLIEBOT_HOME`` selects the profile: unset or empty gives the default
  ``~/.charliebot``, so an untouched host behaves exactly as before. This is the
  only place in the tree that reads the variable; every other path is derived
  from :attr:`CharlieBotConfig.charliebot_home`.

  A set value must be absolute or start with ``~``. A relative value would be
  resolved against each process's own working directory, silently handing the
  server, the CLI and every worker a different home, so it is rejected here
  instead of surfacing later as a write into the wrong profile.
  """
  raw = os.environ.get(CHARLIEBOT_HOME_ENV, "").strip()
  if not raw:
    return default_charliebot_home()
  if not (raw.startswith("~") or raw.startswith("/")):
    raise ValueError(
        f"{CHARLIEBOT_HOME_ENV} must be an absolute path or start with '~'; got {raw!r}")
  return Path(raw).expanduser().resolve()


class ScheduledTaskResolutionError(Exception):
  """Raised when a cron entry cannot be resolved into a ``ScheduledTaskConfig``.

  Covers loader-level resolution failures that must escape the generic reload
  fallback in :func:`get_scheduled_tasks` so the failure is loud and self-healing
  (the scheduler logs and retries each tick) rather than silently serving a
  stale task list: a ``prompt_file`` that is missing/unreadable, or an entry
  carrying both ``prompt`` and ``prompt_file``.
  """


class ImprovementLoopConfig(BaseModel):
  """Declarative config for an improvement-loop cron task."""

  backlog: str  # relative path within repo, e.g. 'backlog/backlog.yaml'
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

  # Kimi (Moonshot) — optional, not wired in by default
  moonshot_api_key: Optional[str] = None

  # Authentication — shared secret; empty string disables auth
  charliebot_access_key: str = ""

  # Server
  server_port: int = 18498

  # Paths — resolved per instantiation so CHARLIEBOT_HOME selects the profile
  charliebot_home: Path = Field(default_factory=charliebot_home_dir)

  # Workspace directories to scan for git repos
  workspace_dirs: list[str] = ["~/workspace"]

  # Root directory for worker worktrees
  worktree_dir: str = "~/worktrees"

  # code-server integration
  code_server_config: str = "configs/code-server.yaml"
  code_server_bin: Optional[str] = None

  # Backlog panel
  backlog_repos: list[BacklogRepoConfig] = []
  backlog_repo: Optional[str] = None  # deprecated, migrated to backlog_repos
  backlog_label: str = 'Project Backlog'  # deprecated, used during migration

  # Subprocess stdout buffer limit in MB (for asyncio StreamReader)
  subprocess_buffer_limit_mb: int = 1024

  # Backend options available for model switching
  # Additional backends (Codex/Gemini/Kimi/Antigravity/etc.) must be configured via
  # ~/.charliebot/config.yaml -> backend_options.
  backend_options: list[BackendOption] = [
      BackendOption(
          id="claude-opus-4.8", label="CC \u00b7 Opus 5", type="cc-claude", model="claude-opus-5", effort="xhigh"),
      BackendOption(
          id="claude-opus-4.8-fast",
          label="CC \u00b7 Opus 4.8 Fast",
          type="cc-claude",
          model="claude-opus-4-8",
          effort="medium",
          fast_mode=True),
      BackendOption(
          id="claude-fable-5", label="CC \u00b7 Fable 5", type="cc-claude", model="claude-fable-5", effort="max"),
      BackendOption(id="claude-tui", label="Claude TUI", type="tui-cli"),
  ]

  # Ordered preference list of BackendOption ids, consumed by two selectors:
  #   - checking-role (reviewer, verify default): first entry that DIFFERS from the
  #     checked party's backend and resolves — see review.select_reviewer_backend.
  #   - light one-shot (autonamer, recap): resolved entries in list order — see
  #     autonamer.iter_light_backends.
  # Empty list (default) skips the one-shot.
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
    return f"http://localhost:{self.server_port}"

  @property
  def sessions_dir(self) -> Path:
    return self.charliebot_home / "sessions"

  @property
  def claude_md_file(self) -> Path:
    """The master agent prompt: ~/.charliebot/MASTER_AGENT_PROMPT.md."""
    return self.charliebot_home / "MASTER_AGENT_PROMPT.md"

  @property
  def memory_dir(self) -> Path:
    """Root of the labeled-entry memory store: ~/.charliebot/memory/."""
    return self.charliebot_home / "memory"

  @property
  def charlie_bot_repo(self) -> Path:
    """Root of the charlie-bot repository (derived from package location)."""
    return Path(__file__).resolve().parents[2]

  @property
  def code_server_config_path(self) -> Path:
    path = Path(self.code_server_config).expanduser()
    if path.is_absolute():
      return path
    return self.charlie_bot_repo / path

  @property
  def code_server_listen_port(self) -> int:
    data = load_yaml(self.code_server_config_path, default={})
    if not isinstance(data, dict):
      raise ValueError(f"code-server config must be a YAML mapping: {self.code_server_config_path}")
    bind_addr = data.get("bind-addr")
    if not isinstance(bind_addr, str) or ":" not in bind_addr:
      raise ValueError(f"code-server config must define bind-addr: {self.code_server_config_path}")
    port_text = bind_addr.rsplit(":", 1)[1]
    try:
      return int(port_text)
    except ValueError as exc:
      raise ValueError(f"code-server bind-addr port must be an integer: {bind_addr}") from exc

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
  """Load config from this profile's ``config.yaml`` (see :func:`charliebot_home_dir`)."""
  home = charliebot_home_dir()
  config_path = home / "config.yaml"

  yaml_data: dict = load_yaml(config_path, default={})

  # The home directory is chosen by the environment, never by the file that lives
  # inside it: honouring the key would leave the config loaded from one profile and
  # the state written to another, and dropping it silently would hide the mistake.
  if "charliebot_home" in yaml_data:
    raise ValueError(
        f"{config_path} sets 'charliebot_home'; that path is chosen by the "
        f"{CHARLIEBOT_HOME_ENV} environment variable. Remove the key.")
  return CharlieBotConfig(charliebot_home=home, **yaml_data)


def get_config() -> CharlieBotConfig:
  """Return the process-wide config, refreshed in place when config.yaml changes.

  The returned instance keeps a stable identity across reloads: holders that
  captured it earlier (manager singletons, in-flight coroutines) observe the new
  values without re-fetching. Replacing the object instead would leave every such
  holder pinned to a stale snapshot.
  """
  global _config, _config_mtime
  config_path = charliebot_home_dir() / "config.yaml"
  try:
    mtime = config_path.stat().st_mtime
  except OSError:
    mtime = 0.0
  if _config is None or mtime != _config_mtime:
    try:
      fresh = load_config()
    except Exception as e:
      log.warning("config_reload_failed", error=str(e))
      if _config is None:
        raise
    else:
      if _config is None:
        _config = fresh
      else:
        # Copy field-by-field from the validated instance; assignment validation is
        # off, so the source must already be a fully validated CharlieBotConfig.
        for name in type(fresh).model_fields:
          setattr(_config, name, getattr(fresh, name))
      _config_mtime = mtime
  return _config


_cron_tasks: list[ScheduledTaskConfig] = []
_cron_mtime: float = 0.0
_cron_prompt_mtimes: dict[Path, float] = {}


def _resolve_prompt_file(entry: dict, repo_root: Path) -> Optional[Path]:
  """Resolve a cron entry's ``prompt_file`` into ``prompt`` in place.

  Reads the referenced file, sets ``entry['prompt']`` to its contents, and
  removes the ``prompt_file`` key so the model never sees it. Returns the
  resolved :class:`Path` (for mtime tracking) or ``None`` if the entry had no
  ``prompt_file``.

  Path resolution has no search order and no shadowing: a relative path is
  resolved against *repo_root* (``cfg.charlie_bot_repo``); a ``~``-prefixed or
  absolute path is taken literally via :func:`os.path.expanduser`.

  Raises :class:`ScheduledTaskResolutionError` if the entry carries both a
  non-empty ``prompt`` and a ``prompt_file`` (two prompt sources is a
  configuration error), or if the file is missing or unreadable.
  """
  pf = entry.get("prompt_file")
  if not pf:
    return None
  if entry.get("prompt"):
    raise ScheduledTaskResolutionError(
        f"cron entry {entry.get('name')!r} has both 'prompt' and 'prompt_file'; "
        "exactly one prompt source is allowed")
  if pf.startswith("~") or Path(pf).is_absolute():
    path = Path(os.path.expanduser(pf))
  else:
    path = repo_root / pf
  try:
    body = path.read_text(encoding="utf-8")
  except OSError as e:
    raise ScheduledTaskResolutionError(f"cron entry {entry.get('name')!r} prompt_file unreadable: {path} ({e})") from e
  entry["prompt"] = body
  del entry["prompt_file"]
  return path


def _detect_local_timezone() -> str:
  """Return the host's IANA timezone, derived from ``/etc/localtime``.

  Resolves the ``/etc/localtime`` symlink to its real path, takes the part after
  ``zoneinfo/``, and validates it with :class:`ZoneInfo`. On any failure (no
  symlink, no ``zoneinfo/`` segment, invalid key) logs one warning and returns
  ``"UTC"`` — this is an environment limitation, not a user configuration error.
  """
  try:
    real = os.path.realpath("/etc/localtime")
    marker = "/zoneinfo/"
    idx = real.rfind(marker)
    if idx < 0:
      raise ValueError(f"no {marker!r} segment in {real!r}")
    tz_name = real[idx + len(marker):]
    if not tz_name:
      raise ValueError(f"empty timezone name in {real!r}")
    ZoneInfo(tz_name)  # validate — raises ZoneInfoNotFoundError on a bad key
    return tz_name
  except Exception as e:
    log.warning("local_timezone_resolve_failed", error=str(e), fallback="UTC")
    return "UTC"


def _resolve_local_timezone(entry: dict) -> None:
  """Rewrite the ``local`` sentinel into the host's IANA zone in place.

  Only entries literally carrying ``timezone: local`` are affected; every other
  value (including the model/API/UI default ``America/Los_Angeles``) is left
  untouched.
  """
  if entry.get("timezone") != "local":
    return
  entry["timezone"] = _detect_local_timezone()


def _stat_prompt_files(paths: dict[Path, float]) -> Optional[dict[Path, float]]:
  """Return current mtimes for *paths*, or ``None`` if any is missing.

  Returning ``None`` forces a reload so a missing ``prompt_file`` surfaces as a
  :class:`ScheduledTaskResolutionError` instead of silently serving a cached
  body from a file that no longer exists.
  """
  current: dict[Path, float] = {}
  for p in paths:
    try:
      current[p] = p.stat().st_mtime
    except OSError:
      return None
  return current


def get_scheduled_tasks() -> list[ScheduledTaskConfig]:
  """Load scheduled tasks from config.d/cron.yaml, with an mtime cache.

  The cache invalidates on the cron file's mtime **and** on the mtime of every
  ``prompt_file`` referenced by the last successful load, so editing a prompt
  ``.md`` (or a ``git pull`` that changes it) takes effect without touching
  ``cron.yaml`` and without a server restart.

  Each entry is resolved before model construction: ``prompt_file`` is read
  into ``prompt`` (and the key removed) and a literal ``timezone: local`` is
  rewritten to the host's IANA zone. Resolution failures raise
  :class:`ScheduledTaskResolutionError`, which escapes the generic reload
  fallback so the failure is loud and self-healing (the scheduler logs and
  retries each tick).
  """
  global _cron_tasks, _cron_mtime, _cron_prompt_mtimes
  cron_path = charliebot_home_dir() / "config.d" / "cron.yaml"
  try:
    mtime = cron_path.stat().st_mtime
  except FileNotFoundError:
    return _cron_tasks
  except OSError as e:
    log.warning("cron_config_stat_failed", error=str(e))
    return _cron_tasks

  # Invalidate when the cron file or any referenced prompt file changed. A
  # previously-referenced prompt file that is now missing (stat fails) forces a
  # reload so the missing-file error surfaces instead of serving a stale body.
  prompt_mtimes = _stat_prompt_files(_cron_prompt_mtimes)
  if (mtime == _cron_mtime and prompt_mtimes is not None and prompt_mtimes == _cron_prompt_mtimes):
    return _cron_tasks

  try:
    data = load_yaml(cron_path, default={})
    raw_tasks = data.get("scheduled_tasks", [])
    repo_root = get_config().charlie_bot_repo
    resolved_prompt_mtimes: dict[Path, float] = {}
    for t in raw_tasks:
      if isinstance(t, dict):
        resolved = _resolve_prompt_file(t, repo_root)
        if resolved is not None:
          try:
            resolved_prompt_mtimes[resolved] = resolved.stat().st_mtime
          except OSError:
            # The file existed moments ago (we just read it); a transient race
            # falls back to a sentinel so the next tick re-reads.
            resolved_prompt_mtimes[resolved] = 0.0
        _resolve_local_timezone(t)
        if t.get("repo"):
          t["repo"] = os.path.expanduser(t["repo"])
    _cron_tasks = [ScheduledTaskConfig(**t) for t in raw_tasks]
    _cron_mtime = mtime
    _cron_prompt_mtimes = resolved_prompt_mtimes
  except ScheduledTaskResolutionError:
    # Configuration error in a prompt_file: fail loud, escape the generic
    # fallback. Scheduler._loop logs and retries each tick, so this self-heals.
    raise
  except Exception as e:
    log.warning("cron_config_reload_failed", error=str(e))
  return _cron_tasks
