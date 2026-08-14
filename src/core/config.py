"""Configuration loading for CharlieBot."""

import os
import re
from pathlib import Path
from typing import Literal, Optional
from zoneinfo import ZoneInfo

import structlog
from pydantic import BaseModel, ConfigDict, Field, model_validator

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
  if not raw.startswith(("~", "/")):
    raise ValueError(
        f"{CHARLIEBOT_HOME_ENV} must be an absolute path or start with '~'; got {raw!r}")
  return Path(raw).expanduser().resolve()


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
  """Configuration for a single scheduled (cron-like) task.

  ``name`` is supplied by the loader (from the host file stem) and is required,
  but the persisted per-job file body never carries ``name``. ``extra='forbid'``
  turns an unknown key (a typo such as ``promt_file:``) into that file's error
  instead of silently dropping it.
  """

  model_config = ConfigDict(extra='forbid')

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
  # Fire mode: absent or 'worker' spawns a worker per fire (existing behavior);
  # 'master' wakes the dedicated session's master with the task's prompt
  # (typically supplied via prompt_file) plus an appended Group line.
  mode: Optional[Literal['worker', 'master']] = None
  allow_failure: bool = False
  notify: Optional[str] = None  # 'telegram' or None

  @model_validator(mode='after')
  def check_prompt_or_handler_or_loop(self) -> 'ScheduledTaskConfig':
    sources = sum([bool(self.prompt), bool(self.handler), bool(self.loop)])
    if sources != 1:
      raise ValueError("task must have exactly one of 'prompt', 'handler', or 'loop'")
    # prompt_file is resolved into prompt before model validation, so an empty
    # prompt here means master woke up with no message at all — including the
    # master+handler and master+loop combinations the exactly-one rule allows.
    if self.mode == 'master' and not self.prompt:
      raise ValueError("mode 'master' requires a prompt source ('prompt' or 'prompt_file')")
    if self.notify and self.notify != 'telegram':
      raise ValueError(f"notify must be 'telegram' or None, got '{self.notify}'")
    if self.mode == 'master' and not self.project:
      raise ValueError("mode 'master' requires 'project' (the group the PM session is bound to)")
    return self


class ScheduledTaskError(BaseModel):
  """A per-file cron load failure surfaced through the API without raising."""

  name: str
  path: str
  error: str


class _CronSnapshot:
  """Module-level cache of the last cron.d load, invalidated by any fingerprint change."""

  __slots__ = ('tasks', 'errors', 'prompt_mtimes', 'fingerprint')

  def __init__(self) -> None:
    self.tasks: list[ScheduledTaskConfig] = []
    self.errors: list[ScheduledTaskError] = []
    self.prompt_mtimes: dict[Path, float] = {}
    self.fingerprint: object = None


class BacklogRepoConfig(BaseModel):
  """A single backlog repo entry: label + path."""
  label: str
  path: str


class HomeService(BaseModel):
  """An externally-hosted service listed on the home page."""
  name: str  # card title
  description: str  # one line saying what it is for
  url: str  # what the card links to; the probe connects to this URL's host and port


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
  # Home page — externally-hosted services probed for reachability; default empty
  home_services: list[HomeService] = []
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

  # Slack summon entrypoint
  slack_bot_token: Optional[str] = None       # xoxb-…, chat:write + history scopes
  slack_app_token: Optional[str] = None       # xapp-…, connections:write, Socket Mode only
  slack_allowed_user_ids: list[str] = []      # Slack user ids allowed to summon; empty = nobody

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


_cron_snapshot = _CronSnapshot()


def _resolve_prompt_file(entry: dict, repo_root: Path) -> Optional[Path]:
  """Resolve a cron entry's ``prompt_file`` into ``prompt`` in place.

  Reads the referenced file, sets ``entry['prompt']`` to its contents, and
  removes the ``prompt_file`` key so the model never sees it. Returns the
  resolved :class:`Path` (for mtime tracking) or ``None`` if the entry had no
  ``prompt_file``.

  Path resolution has no search order and no shadowing: a relative path is
  resolved against *repo_root* (``cfg.charlie_bot_repo``); a ``~``-prefixed or
  absolute path is taken literally via :func:`os.path.expanduser`.

  Raises :class:`ValueError` if the entry carries both a non-empty ``prompt``
  and a ``prompt_file`` (two prompt sources is a configuration error), or if the
  file is missing or unreadable.
  """
  pf = entry.get("prompt_file")
  if not pf:
    return None
  if entry.get("prompt"):
    raise ValueError(
        f"cron entry {entry.get('name')!r} has both 'prompt' and 'prompt_file'; "
        "exactly one prompt source is allowed")
  if pf.startswith("~") or Path(pf).is_absolute():
    path = Path(os.path.expanduser(pf))
  else:
    path = repo_root / pf
  try:
    body = path.read_text(encoding="utf-8")
  except OSError as e:
    raise ValueError(f"cron entry {entry.get('name')!r} prompt_file unreadable: {path} ({e})") from e
  entry["prompt"] = body
  del entry["prompt_file"]
  return path


def claude_config_dir(option: BackendOption) -> Path:
  """Resolve the CLAUDE_CONFIG_DIR a cc-claude process will use.

  Single source of truth for the resume-domain resolution order: the option's
  ``claude_config_dir`` override, then ``$CLAUDE_CONFIG_DIR``, then
  ``~/.claude``. Both the API backend-switch guard and the runtime resume
  resolver call this — do not restate the order anywhere else.
  """
  if option.claude_config_dir:
    return Path(option.claude_config_dir).expanduser()
  env_dir = os.environ.get("CLAUDE_CONFIG_DIR")
  if env_dir:
    return Path(env_dir).expanduser()
  return Path.home() / ".claude"


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
  per-file load error instead of silently serving a cached body from a file that
  no longer exists.
  """
  current: dict[Path, float] = {}
  for p in paths:
    try:
      current[p] = p.stat().st_mtime
    except OSError:
      return None
  return current


def _cron_d_dir() -> Path:
  """Path of this profile's per-job cron directory. Resolved per call."""
  return charliebot_home_dir() / "config.d" / "cron.d"


def _legacy_cron_file() -> Path:
  """Path of the legacy single-file cron config (a tripwire, never a fallback)."""
  return charliebot_home_dir() / "config.d" / "cron.yaml"


def _valid_cron_name(name: str) -> bool:
  """Return whether *name* is a safe cron job name for a single host file.

  Only names matching ``^[A-Za-z0-9][A-Za-z0-9._-]*$`` are safe: anything else
  (``..``, an embedded ``/``, a leading ``.``, an absolute-looking name) could
  escape the ``cron.d`` directory, so it is rejected with a 400 before any
  filesystem access. The host also uses this guard when enumerating files.
  """
  return bool(re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", name))


def _validate_cron_body(body: dict, repo: Path, stem: str) -> tuple[ScheduledTaskConfig, dict[Path, float]]:
  """Resolve and validate one cron job body into a ``ScheduledTaskConfig``.

  Mutates *body* in place — resolves ``prompt_file`` (removing the key and
  setting ``prompt``), rewrites a literal ``timezone: local`` to the host IANA
  zone, and expands ``~`` in ``repo`` — matching the :func:`_resolve_prompt_file`
  mutate-in-place convention. This is the exact body-processing step
  :func:`_load_cron_file` uses for the production loader; any other caller
  (e.g. the cron API's update path) that validates a candidate through this
  function is guaranteed a result the loader can reload unchanged.

  Returns the model plus the mtime map for any ``prompt_file`` it read (for the
  hot-reload fingerprint). Raises :class:`ValueError` (or a pydantic validation
  error) on any failure.
  """
  prompt_mtimes: dict[Path, float] = {}
  prompt_file = body.get("prompt_file")
  if prompt_file:
    resolved = _resolve_prompt_file(body, repo)
    try:
      prompt_mtimes[resolved] = resolved.stat().st_mtime
    except OSError:
      # The file existed moments ago (we just read it); a transient race falls
      # back to a sentinel so the next tick re-reads.
      prompt_mtimes[resolved] = 0.0
  _resolve_local_timezone(body)
  if body.get("repo"):
    body["repo"] = os.path.expanduser(body["repo"])
  return ScheduledTaskConfig(name=stem, **body), prompt_mtimes


def _load_cron_file(path: Path, repo: Path, stem: str) -> tuple[ScheduledTaskConfig, dict[Path, float]]:
  """Load, resolve, and validate one ``cron.d`` file into a ``ScheduledTaskConfig``.

  The file body is a top-level mapping of :class:`ScheduledTaskConfig` fields
  *without* ``name``; the job name is *stem* (the file stem) and is injected
  here. A body that carries a ``name`` key is an error (the file name is the
  single source of the name). Resolution follows the existing order:
  ``prompt_file`` against *repo* (``~``-prefixed or absolute taken literally),
  ``timezone: local`` to the host IANA zone, and ``repo`` to ``expanduser`` —
  see :func:`_validate_cron_body`.

  Returns the model plus the mtime map for any ``prompt_file`` it read (for the
  hot-reload fingerprint). Raises :class:`ValueError` on any failure; the caller
  records it as a per-file error rather than propagating it.
  """
  body = load_yaml(path)
  if not isinstance(body, dict):
    raise ValueError("cron config must be a mapping")
  if "name" in body:
    raise ValueError("the body must not carry a 'name' key; the file name is the job name")
  return _validate_cron_body(body, repo, stem)


def _reload_cron_snapshot() -> _CronSnapshot:
  """Recompute the snapshot by loading every ``cron.d`` file independently."""
  global _cron_snapshot
  repo = get_config().charlie_bot_repo
  cron_d = _cron_d_dir()
  legacy_file = _legacy_cron_file()

  tasks: list[ScheduledTaskConfig] = []
  errors: list[ScheduledTaskError] = []
  prompt_mtimes: dict[Path, float] = {}

  # A missing cron.d/ directory is an empty set, not an error.
  if cron_d.is_dir():
    for path in sorted(cron_d.iterdir()):
      if not (path.is_file() and path.name.endswith(".yaml") and not path.name.startswith(".")):
        continue
      stem = path.stem
      if not _valid_cron_name(stem):
        errors.append(ScheduledTaskError(
            name=stem,
            path=str(path),
            error="file name is not a valid cron task name"))
        continue
      try:
        task, file_prompt_mtimes = _load_cron_file(path, repo, stem)
      except Exception as e:
        errors.append(ScheduledTaskError(name=stem, path=str(path), error=str(e)))
        log.error("cron_task_load_failed", name=stem, path=str(path), error=str(e))
        continue
      tasks.append(task)
      prompt_mtimes.update(file_prompt_mtimes)

  # Legacy tripwire: a leftover config.d/cron.yaml is a loud error, never a
  # silent fallback. None of its entries are loaded.
  if legacy_file.exists():
    errors.append(ScheduledTaskError(
        name="cron.yaml (legacy)",
        path=str(legacy_file),
        error="legacy config.d/cron.yaml present; entries not loaded — "
              "split into config.d/cron.d/<name>.yaml"))
    log.error("cron_legacy_file_present", path=str(legacy_file))

  tasks.sort(key=lambda t: t.name)
  errors.sort(key=lambda e: e.name)
  snapshot = _CronSnapshot()
  snapshot.tasks = tasks
  snapshot.errors = errors
  snapshot.prompt_mtimes = prompt_mtimes
  snapshot.fingerprint = _cron_fingerprint(prompt_mtimes)
  _cron_snapshot = snapshot
  return snapshot


def _cron_fingerprint(prompt_mtimes: dict[Path, float]):
  """Compute the hot-reload fingerprint over all four re-read inputs.

  The set of ``cron.d/*.yaml`` paths with each file's mtime, the mtime of every
  referenced ``prompt_file`` (a referenced file that has gone missing makes the
  stat fail, returning ``None`` and forcing a full re-read so the failure
  surfaces instead of a stale cached body), and whether the legacy
  ``config.d/cron.yaml`` exists.
  """
  cron_d = _cron_d_dir()
  legacy_file = _legacy_cron_file()
  files: list[tuple[str, float]] = []
  if cron_d.is_dir():
    for p in sorted(cron_d.iterdir()):
      if p.is_file() and p.name.endswith(".yaml") and not p.name.startswith("."):
        try:
          files.append((p.name, p.stat().st_mtime))
        except OSError:
          files.append((p.name, 0.0))
  current_prompt_mtimes = _stat_prompt_files(prompt_mtimes)
  return (tuple(files), current_prompt_mtimes, legacy_file.exists())


def get_scheduled_tasks() -> list[ScheduledTaskConfig]:
  """Load the valid scheduled tasks from ``config.d/cron.d/<name>.yaml``.

  Total and never raises: any file that fails to parse or validate becomes a
  single entry in :func:`get_scheduled_task_errors` and is skipped, every other
  file still loads and is schedulable. The result is sorted by name.

  The snapshot refreshes whenever the fingerprint changes (cron.d file set and
  mtimes, referenced prompt_file mtimes, and legacy-presence), so a change takes
  effect on the next call with no restart.
  """
  return _refresh_cron_snapshot().tasks


def get_scheduled_task_errors() -> list[ScheduledTaskError]:
  """Return one record per failing cron job file, sorted by name.

  Total and never raises, mirroring :func:`get_scheduled_tasks`. Includes the
  legacy-tripwire record when ``config.d/cron.yaml`` exists.
  """
  return _refresh_cron_snapshot().errors


def _refresh_cron_snapshot() -> _CronSnapshot:
  """Return the cached snapshot, reloading when the fingerprint changed."""
  snapshot = _cron_snapshot
  if snapshot.fingerprint == _cron_fingerprint(snapshot.prompt_mtimes):
    return snapshot
  return _reload_cron_snapshot()
