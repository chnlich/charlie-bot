"""Configuration loading for CharlieBot."""

import asyncio
import json
import os
import re
from pathlib import Path
from typing import Literal
from zoneinfo import ZoneInfo

import structlog
from pydantic import BaseModel, ConfigDict, Field, model_validator

from src.core.models import BackendOption
from src.core.tasks import create_logged_task
from src.core.yaml_utils import load_yaml

log = structlog.get_logger()

CHARLIEBOT_HOME_ENV = "CHARLIEBOT_HOME"

# Fixed house wall clock pinned by chart timestamps (src/api/pages.py), Slack timestamp
# prefixes (src/core/slack_listener.py), worker-summary timestamps
# (src/core/spawner_events.py), and the Saturday-1AM weekly-recycle anchor
# (src/core/master_trigger.py). Distinct from DEFAULT_TIMEZONE below, a per-task default
# overridable via ``timezone: local`` or any IANA key, so retargeting the cron default
# cannot shift these pins.
HOUSE_TIMEZONE = "America/Los_Angeles"

# The API request model TaskCreate (src/api/cron.py) shares this default; the web UI
# re-pins the value in three literals (templates/index.html, two fallbacks in
# sidebar/modals.js) that cannot import from Python — a change moves all four sites.
DEFAULT_TIMEZONE = HOUSE_TIMEZONE


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


# Single home of the mode:'master' project invariant: the cron create route
# reports the violation as a 400 while the model validator raises it, so the
# condition and message must not be restated per layer.
def master_task_project_error(mode: str | None, project: str | None) -> str | None:
  """Return the error text when a mode: master task lacks a project, else None."""
  if mode == 'master' and not project:
    return "mode 'master' requires 'project' (the group the PM session is bound to)"
  return None


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
  prompt: str | None = None
  # Pre-resolution path string a host cron.d file declared. It is an in-process
  # field for transport to the API and UI only; no write path persists it.
  prompt_file: str | None = None
  handler: str | None = None
  loop: ImprovementLoopConfig | None = None
  repo: str | None = None
  backend: str | None = None
  timezone: str = DEFAULT_TIMEZONE
  enabled: bool = True
  project: str | None = None
  # Fire mode: absent or 'worker' spawns a worker per fire (existing behavior);
  # 'master' wakes the dedicated session's master with the task's prompt: the
  # pointed file owns the body, the host cron file carries only its path, and
  # the loader reads the file on every load. An appended Group line follows the
  # prompt.
  mode: Literal['worker', 'master'] | None = None
  allow_failure: bool = False
  notify: str | None = None  # 'telegram' or None

  @model_validator(mode='after')
  def check_prompt_or_handler_or_loop(self) -> 'ScheduledTaskConfig':
    sources = sum([bool(self.prompt), bool(self.handler), bool(self.loop)])
    if sources != 1:
      raise ValueError(
          "task must have exactly one of 'prompt', 'prompt_file', 'handler', or 'loop'")
    # A prompt_file-style entry is resolved into prompt before model
    # validation, so an empty prompt here means master woke up with no message
    # at all — including the master+handler and master+loop combinations the
    # exactly-one rule allows.
    if self.mode == 'master' and not self.prompt:
      raise ValueError("mode 'master' requires a prompt source ('prompt' or 'prompt_file')")
    if self.notify and self.notify != 'telegram':
      raise ValueError(f"notify must be 'telegram' or None, got '{self.notify}'")
    if project_error := master_task_project_error(self.mode, self.project):
      raise ValueError(project_error)
    return self


class ScheduledTaskError(BaseModel):
  """A per-file cron load failure surfaced through the API without raising.

  ``enabled`` is the failing file's own raw ``enabled`` value, read best-effort
  at load-failure time — ``None`` when the body cannot be parsed at all (a
  syntax-error yaml gives no truthful answer, and guessing "on" would
  misstate the file).
  """

  name: str
  path: str
  error: str
  enabled: bool | None = None


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
  moonshot_api_key: str | None = None

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
  code_server_bin: str | None = None

  # Plan registration page-height gate — absolute path of a headless-chromium-compatible
  # binary on the host running the server. The value stays host-local in config.yaml;
  # nothing in the repo hardcodes a path.
  headless_chrome_bin: str = ""

  # Backlog panel
  backlog_repos: list[BacklogRepoConfig] = []
  # Home page — externally-hosted services probed for reachability; default empty
  home_services: list[HomeService] = []
  backlog_repo: str | None = None  # deprecated, migrated to backlog_repos
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
  telegram_bot_token: str | None = None
  telegram_chat_id: str | None = None

  # Slack summon entrypoint
  slack_bot_token: str | None = None          # xoxb-…, chat:write + history scopes
  slack_app_token: str | None = None          # xapp-…, connections:write, Socket Mode only
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

  def get_backend_option(self, backend_id: str) -> BackendOption | None:
    """Look up a backend option by id."""
    return next((opt for opt in self.backend_options if opt.id == backend_id), None)

  def discover_repos(self) -> list[dict[str, str]]:
    """Scan workspace_dirs for directories containing a .git folder."""
    repos: list[dict[str, str]] = []
    for dir_str in self.workspace_dirs:
      parent = Path(dir_str)
      if not parent.is_dir():
        continue
      repos.extend(
          {"name": child.name, "path": str(child)}
          for child in sorted(parent.iterdir())
          if child.is_dir() and (child / ".git").exists())
    return repos


_config: CharlieBotConfig | None = None
# The last fingerprint _config_fingerprint() returned for the cached config; tests
# assign a sentinel (e.g. 0.0) to force a reload.
_config_mtime: object = None


def _config_fragments(home: Path) -> list[Path]:
  """config.d/*.yaml, sorted by name; dotfiles and the legacy cron.yaml excluded."""
  config_d = home / "config.d"
  if not config_d.is_dir():
    return []
  return sorted(
      (p for p in config_d.glob("*.yaml")
       if p.is_file() and not p.name.startswith(".") and p.name != "cron.yaml"),
      key=lambda p: p.name)


def _stat_mtime_size(path: Path) -> tuple[float, int]:
  """``(mtime, size)`` for *path*; a missing file contributes a fixed sentinel."""
  try:
    st = path.stat()
  except OSError:
    return (0.0, 0)
  return (st.st_mtime, st.st_size)


def _config_fingerprint() -> tuple:
  """The reload cache key over ``config.yaml`` and every config fragment.

  ``config.yaml``'s ``(mtime, size)`` plus ``(name, mtime, size)`` for each
  fragment in name order. Size comes from the same stat call and costs nothing
  extra; it catches mtime-preserving writes (``cp -p``, ``touch -r``, two writes
  inside one second on a coarse-resolution filesystem) that an mtime-only key
  would miss silently. A content change that preserves both mtime and size is
  deliberately not covered. A missing file stats to a sentinel rather than
  raising, mirroring the old mtime-only key.
  """
  home = charliebot_home_dir()
  return (
      _stat_mtime_size(home / "config.yaml"),
      tuple((p.name, *_stat_mtime_size(p)) for p in _config_fragments(home)),
  )


def load_config() -> CharlieBotConfig:
  """Load config from this profile's ``config.yaml`` and ``config.d`` fragments.

  The merged mapping starts from ``config.yaml``'s top-level mapping; each
  fragment from :func:`_config_fragments` then merges its own top-level mapping
  in file-name order. The merge is shallow and disjoint: a top-level key belongs
  entirely to the one file that sets it, and a key defined in two files raises
  naming the key and both paths — there is no override precedence. With no
  ``config.d/`` directory (or an empty one) the result is exactly what loading
  ``config.yaml`` alone gives.
  """
  home = charliebot_home_dir()
  config_path = home / "config.yaml"

  yaml_data: dict = load_yaml(config_path, default={})
  key_origin: dict[str, Path] = dict.fromkeys(yaml_data, config_path)
  for fragment in _config_fragments(home):
    fragment_data: dict = load_yaml(fragment, default={})
    if not isinstance(fragment_data, dict):
      raise ValueError(f"config fragment must be a top-level mapping: {fragment}")
    for key, value in fragment_data.items():
      if key in key_origin:
        raise ValueError(
            f"config key {key!r} is defined in both {key_origin[key]} and {fragment}; "
            "a top-level key must live in exactly one file")
      key_origin[key] = fragment
      yaml_data[key] = value

  # The home directory is chosen by the environment, never by a file that lives
  # inside it: honouring the key would leave the config loaded from one profile and
  # the state written to another, and dropping it silently would hide the mistake.
  if "charliebot_home" in yaml_data:
    raise ValueError(
        f"{key_origin['charliebot_home']} sets 'charliebot_home'; that path is chosen by the "
        f"{CHARLIEBOT_HOME_ENV} environment variable. Remove the key.")
  return CharlieBotConfig(charliebot_home=home, **yaml_data)


def get_config() -> CharlieBotConfig:
  """Return the process-wide config, refreshed in place when any config file changes.

  The reload key covers ``config.yaml`` and every ``config.d`` fragment — see
  :func:`_config_fingerprint`. The returned instance keeps a stable identity
  across reloads: holders that captured it earlier (manager singletons,
  in-flight coroutines) observe the new values without re-fetching. Replacing
  the object instead would leave every such holder pinned to a stale snapshot.
  """
  global _config, _config_mtime
  fingerprint = _config_fingerprint()
  if _config is None or fingerprint != _config_mtime:
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
      _config_mtime = fingerprint
  return _config


_cron_snapshot = _CronSnapshot()


def _resolve_prompt_file(entry: dict, repo_root: Path) -> Path | None:
  """Resolve a cron entry's ``prompt_file`` into ``prompt`` in place.

  Reads the referenced file, sets ``entry['prompt']`` to its contents, and
  removes the ``prompt_file`` key while resolving. Callers that expose the
  runtime model restore the raw pointer after this step. Returns the resolved
  :class:`Path` (for mtime tracking) or ``None`` if the entry had no
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


def _stat_prompt_files(paths: dict[Path, float]) -> dict[Path, float] | None:
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


def cron_dir() -> Path:
  """Path of this profile's per-job cron config directory. Resolved per call."""
  return charliebot_home_dir() / "config.d" / "cron.d"


def cron_path(name: str) -> Path:
  """Path of one job's cron config file. Resolved per call, never at import."""
  return cron_dir() / f"{name}.yaml"


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

  Mutates *body* in place — resolves ``prompt_file`` (setting ``prompt`` to the
  referenced file's body and preserving the raw pointer on the model), rewrites
  a literal ``timezone: local`` to the host IANA zone, and expands ``~`` in
  ``repo`` — matching the :func:`_resolve_prompt_file` mutate-in-place
  convention. The pointer owns the prompt body; the body carries only the path
  to it, and the loader reads that file on every load. Any caller that needs
  the pre-write file format (e.g. the cron API's create/update paths, which
  validate a deep copy) is guaranteed a result the loader can reload unchanged.

  Returns the model plus the mtime map for any ``prompt_file`` it read (for the
  hot-reload fingerprint). Raises :class:`ValueError` (or a pydantic validation
  error) on any failure.
  """
  prompt_mtimes: dict[Path, float] = {}
  prompt_file = body.get("prompt_file")
  if prompt_file:
    resolved = _resolve_prompt_file(body, repo)
    body["prompt_file"] = prompt_file  # preserve the raw pointer for the API/UI
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
  single source of the name). A host file carries the path to its prompt source
  under ``prompt_file``; the pointed file owns the body, and this loader reads
  it on every load. A body that instead carries the body itself under ``prompt``
  holds a second source, so it is a load error. Resolution follows the existing
  order: ``prompt_file`` against *repo* (``~``-prefixed or absolute taken
  literally), ``timezone: local`` to the host IANA zone, and ``repo`` to
  ``expanduser`` — see :func:`_validate_cron_body`.

  Returns the model plus the mtime map for any ``prompt_file`` it read (for the
  hot-reload fingerprint). Raises :class:`ValueError` on any failure; the caller
  records it as a per-file error rather than propagating it.
  """
  body = load_yaml(path)
  if not isinstance(body, dict):
    raise ValueError("cron config must be a mapping")
  if "name" in body:
    raise ValueError("the body must not carry a 'name' key; the file name is the job name")
  if "prompt" in body:
    raise ValueError(
        "a cron.d host file must not carry an inline 'prompt'; it holds the "
        "path to the prompt source under 'prompt_file', and the loader reads "
        "that file on every load")
  return _validate_cron_body(body, repo, stem)


def _read_cron_file_enabled(path: Path) -> bool | None:
  """Best-effort raw ``enabled`` read of a cron file the loader failed on.

  Returns the file's own ``enabled`` when the body parses as a mapping with a
  boolean value; ``None`` on any read/parse failure — an unparseable file has
  no truthful raw value, and inventing a default would misstate it.
  """
  try:
    body = load_yaml(path)
  except Exception as e:
    log.debug("cron_file_enabled_unreadable", path=str(path), error=str(e))
    return None
  if isinstance(body, dict) and isinstance(body.get("enabled"), bool):
    return body["enabled"]
  return None


def _reload_cron_snapshot() -> _CronSnapshot:
  """Recompute the snapshot by loading every ``cron.d`` file independently."""
  global _cron_snapshot
  repo = get_config().charlie_bot_repo
  cron_d = cron_dir()
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
            error="file name is not a valid cron task name",
            enabled=_read_cron_file_enabled(path)))
        continue
      try:
        task, file_prompt_mtimes = _load_cron_file(path, repo, stem)
      except Exception as e:
        # Keep a failed pointer in the fingerprint too. If its target is
        # restored without touching the host yaml, the next call must retry
        # the file and clear the error instead of serving a cached failure.
        try:
          failed_body = load_yaml(path)
        except Exception as read_error:
          log.debug("cron_failed_file_prompt_path_unreadable", path=str(path), error=str(read_error))
        else:
          failed_prompt_file = failed_body.get("prompt_file") if isinstance(failed_body, dict) else None
          if isinstance(failed_prompt_file, str) and failed_prompt_file:
            if failed_prompt_file.startswith("~") or Path(failed_prompt_file).is_absolute():
              failed_prompt_path = Path(os.path.expanduser(failed_prompt_file))
            else:
              failed_prompt_path = repo / failed_prompt_file
            try:
              prompt_mtimes[failed_prompt_path] = failed_prompt_path.stat().st_mtime
            except OSError:
              prompt_mtimes[failed_prompt_path] = 0.0
            except ValueError as stat_error:
              log.debug("cron_failed_prompt_path_unstatable", path=str(failed_prompt_path), error=str(stat_error))
        errors.append(ScheduledTaskError(
            name=stem, path=str(path), error=str(e), enabled=_read_cron_file_enabled(path)))
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
  _fire_cron_error_alert([e.name for e in errors])
  snapshot = _CronSnapshot()
  snapshot.tasks = tasks
  snapshot.errors = errors
  snapshot.prompt_mtimes = prompt_mtimes
  snapshot.fingerprint = _cron_fingerprint(prompt_mtimes)
  _cron_snapshot = snapshot
  return snapshot


def _cron_alert_state_path() -> Path:
  """Path of the persisted last-alerted cron error fingerprint. Resolved per call."""
  return charliebot_home_dir() / "state" / "cron_alert_fingerprint.json"


def _read_cron_alert_state() -> frozenset[str]:
  """The last-alerted set of broken cron task names persisted on disk.

  A missing file reads as the empty set: on first deployment any currently
  broken task counts as a fresh non-empty transition and alerts once. A
  corrupt or unreadable file also reads as empty (alerting again beats never
  alerting), with a warning.
  """
  try:
    raw = _cron_alert_state_path().read_text(encoding="utf-8")
  except FileNotFoundError:
    return frozenset()
  except OSError as e:
    log.warning("cron_alert_state_unreadable", error=str(e))
    return frozenset()
  try:
    data = json.loads(raw)
  except ValueError as e:
    log.warning("cron_alert_state_unparseable", error=str(e))
    return frozenset()
  if not isinstance(data, list):
    log.warning("cron_alert_state_unparseable", error="state file is not a JSON list")
    return frozenset()
  return frozenset(str(name) for name in data)


def _fire_cron_error_alert(error_names: list[str]) -> None:
  """Alert over Telegram on every transition of the broken-cron-task name set.

  Compares the fresh set against the last-alerted set persisted at
  :func:`_cron_alert_state_path`; on any difference it records the new set and
  fires one notification — ``"⚠️ cron 任务加载失败: <names>"`` when the new set
  is non-empty, ``"✅ cron 加载失败已全部解除"`` when it turned empty (recovery
  fires only on the full transition, not on every shrink); an identical set
  stays silent.

  The send is fire-and-forget through
  :func:`src.core.tasks.create_logged_task`, so a Telegram failure is a logged
  background-task failure and can never raise back into the config loader or
  the scheduler tick. With no running event loop (a synchronous CLI path) the
  send is skipped and the new set left unpersisted, so the next looped
  evaluation — the scheduler's unconditional 60s tick through
  :func:`get_scheduled_tasks` — transitions again and fires.
  """
  new_set = frozenset(error_names)
  if new_set == _read_cron_alert_state():
    return
  try:
    asyncio.get_running_loop()
  except RuntimeError:
    log.info("cron_alert_skipped_no_event_loop", names=sorted(new_set))
    return
  names = sorted(new_set)
  if names:
    message = "⚠️ cron 任务加载失败: " + ", ".join(names)
  else:
    message = "✅ cron 加载失败已全部解除"
  try:
    # Lazy: notifications imports this module.
    from src.core.notifications import send_telegram

    create_logged_task(send_telegram(message, get_config()), name="cron-load-alert")
  except Exception:
    log.exception("cron_alert_dispatch_failed", names=names)
  try:
    state_path = _cron_alert_state_path()
    state_path.parent.mkdir(parents=True, exist_ok=True)
    state_path.write_text(json.dumps(names, ensure_ascii=False) + "\n", encoding="utf-8")
  except OSError:
    log.exception("cron_alert_state_write_failed")


def _cron_fingerprint(prompt_mtimes: dict[Path, float]):
  """Compute the hot-reload fingerprint over all four re-read inputs.

  The set of ``cron.d/*.yaml`` paths with each file's mtime, the mtime of every
  referenced ``prompt_file`` (a referenced file that has gone missing makes the
  stat fail, returning ``None`` and forcing a full re-read so the failure
  surfaces instead of a stale cached body), and whether the legacy
  ``config.d/cron.yaml`` exists.
  """
  cron_d = cron_dir()
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
