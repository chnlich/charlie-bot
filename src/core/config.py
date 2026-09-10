"""Configuration loading for CharlieBot."""

import asyncio
import json
import os
import re
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path
from typing import Generic, Literal, TypeVar
from zoneinfo import ZoneInfo

import structlog
from pydantic import (
    AliasChoices,
    AliasPath,
    BaseModel,
    ConfigDict,
    Field,
    ValidationError,
    model_validator,
)

from src.core.log_once import WarnOnceRegistry
from src.core.models import (
    BackendOption,
    ClaudeAccount,
    ClaudeCompactionConfig,
)
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

# The API request model TaskCreate (src/api/cron.py) inherits this default through
# ScheduledTaskFields; the web UI re-pins the value in three literals
# (templates/index.html, two fallbacks in sidebar/modals.js) that cannot import
# from Python — a change moves all three sites.
DEFAULT_TIMEZONE = HOUSE_TIMEZONE

# The resolved home and its string form, per raw ``CHARLIEBOT_HOME`` value plus
# ``HOME`` (``""`` raw is the default home, and a ``~`` value derives from
# HOME). The env values are the process's profile identity, fixed for the
# process life, while resolve() is a per-component symlink walk and
# ``Path.home()``/``str(Path)`` re-parse the path — per-request-fingerprint
# work on every call if repeated. Both public readers serve the same cached
# entry, so a caller comparing its home against the default sees one answer.
_home_cache: dict[tuple[str, str], tuple[Path, str]] = {}


def _home_cached(raw: str) -> tuple[Path, str]:
  """The home for *raw* (``""`` is the default) as ``(Path, str)``, resolved once per env pair."""
  key = (raw, os.environ.get("HOME", ""))
  cached = _home_cache.get(key)
  if cached is None:
    if raw:
      home = Path(raw).expanduser().resolve()
    else:
      home = Path.home() / ".charliebot"
    cached = (home, str(home))
    _home_cache[key] = cached
  return cached


def _resolve_home() -> tuple[Path, str]:
  """The validated profile home as ``(Path, str)``."""
  raw = os.environ.get(CHARLIEBOT_HOME_ENV, "").strip()
  if raw and not raw.startswith(("~", "/")):
    raise ValueError(f"{CHARLIEBOT_HOME_ENV} must be an absolute path or start with '~'; got {raw!r}")
  return _home_cached(raw)


def default_charliebot_home() -> Path:
  """The state directory used when ``CHARLIEBOT_HOME`` is unset."""
  return _home_cached("")[0]


def charliebot_home_dir() -> Path:
  """Return the state directory this process belongs to (its profile).

  ``CHARLIEBOT_HOME`` selects the profile: unset or empty gives the default
  ``~/.charliebot``, so an untouched host behaves exactly as before. This is the
  only place that resolves the home path; every other path is derived
  from :attr:`CharlieBotConfig.charliebot_home`. The one raw read of the variable
  outside this function is the web terminal's profile check
  (``src/agents/backends/terminal.py``): a tmux pane inherits the tmux server's
  environment rather than this process's, so the terminal checks whether a
  profile is set and passes the resolved home to new panes explicitly.

  A set value must be absolute or start with ``~``. A relative value would be
  resolved against each process's own working directory, silently handing the
  server, the CLI and every worker a different home, so it is rejected here
  instead of surfacing later as a write into the wrong profile.
  """
  return _resolve_home()[0]


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


class StepConfig(BaseModel):
  """One step of a ``steps`` cron task: a named worker in an ordered chain.

  ``prompt_file`` is the pre-resolution path string the host cron.d file
  declared — an in-process field for transport to the API and UI only, exactly
  like the task-level ``prompt_file``; ``prompt`` is the body the loader
  resolved from it on this load.
  """

  model_config = ConfigDict(extra='forbid')

  name: str = Field(min_length=1)
  prompt_file: str | None = None
  prompt: str | None = None
  backend: str | None = None


class ScheduledTaskFields(BaseModel):
  """Field block every scheduled task carries, shared by the loader's task model
  and the API's create-request model so a new task field ships to both with one edit.

  pydantic merges a parent's config into each child, so every subclass pins its
  own extra-keys policy: the loader model rejects unknown keys
  (``extra='forbid'``), the create-request body keeps ignoring them
  (``extra='ignore'``).
  """

  name: str
  cron: str
  # Pre-resolution path string a host cron.d file declared. It is an in-process
  # field for transport to the API and UI only; no write path persists it.
  prompt_file: str | None = None
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


class ScheduledTaskConfig(ScheduledTaskFields):
  """Configuration for a single scheduled (cron-like) task.

  ``name`` is supplied by the loader (from the host file stem) and is required,
  but the persisted per-job file body never carries ``name``. ``extra='forbid'``
  turns an unknown key (a typo such as ``promt_file:``) into that file's error
  instead of silently dropping it.
  """

  model_config = ConfigDict(extra='forbid')

  prompt: str | None = None
  handler: str | None = None
  loop: ImprovementLoopConfig | None = None
  # Ordered worker chain: each step spawns after the previous one exits 0, and
  # the session master is woken once at the end (src/core/task_chain.py).
  steps: list[StepConfig] | None = None
  notify: str | None = None  # 'telegram' or None

  @model_validator(mode='after')
  def check_prompt_or_handler_or_loop(self) -> 'ScheduledTaskConfig':
    sources = sum([bool(self.prompt), bool(self.steps), bool(self.handler), bool(self.loop)])
    if sources != 1:
      raise ValueError("task must have exactly one of 'prompt', 'prompt_file', 'steps', 'handler', or 'loop'")
    if self.steps is not None and not self.steps:
      raise ValueError("steps must be a non-empty list")
    if self.steps:
      seen: set[str] = set()
      for step in self.steps:
        if step.name in seen:
          raise ValueError(f"duplicate step name '{step.name}'")
        seen.add(step.name)
        if not step.prompt:
          raise ValueError(
              f"step '{step.name}' has no prompt body; the loader resolves each step's "
              "'prompt_file' before validation")
    # A prompt_file-style entry is resolved into prompt before model
    # validation, so an empty prompt here means master woke up with no message
    # at all — including the master+handler and master+loop combinations the
    # exactly-one rule allows.
    if self.mode == 'master' and self.steps:
      raise ValueError("mode 'master' requires a prompt source ('prompt' or 'prompt_file'), not 'steps'")
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

  __slots__ = ('errors', 'fingerprint', 'prompt_mtimes', 'tasks')

  def __init__(self) -> None:
    self.tasks: list[ScheduledTaskConfig] = []
    self.errors: list[ScheduledTaskError] = []
    self.prompt_mtimes: dict[Path, float] = {}
    self.fingerprint: object = None


class BacklogRepoConfig(BaseModel):
  """A single backlog repo entry: label + path."""

  model_config = ConfigDict(extra='forbid')

  label: str
  path: str


class HomeService(BaseModel):
  """A service this host runs, listed on the /home page and probed for reachability."""

  model_config = ConfigDict(extra='forbid')

  name: str  # card title
  description: str  # one line saying what it is for
  url: str  # what the card links to; the probe connects to this URL's host and port


class ServerConfig(BaseModel):
  """``server:`` section: the bind address uvicorn listens on."""

  model_config = ConfigDict(extra='forbid')

  # The bind address uvicorn listens on. Loopback by default; a host that
  # fronts the server itself (reverse proxy on another interface, Tailscale) sets it.
  host: str = "127.0.0.1"
  port: int = 18498

  # Subprocess stdout buffer limit in MB (for asyncio StreamReader)
  subprocess_buffer_limit_mb: int = 1024


class PathsConfig(BaseModel):
  """``paths:`` section: repos to scan and where worker worktrees live."""

  model_config = ConfigDict(extra='forbid')

  # Workspace directories to scan for git repos
  workspace_dirs: list[str] = ["~/workspace"]

  # Root directory for worker worktrees
  worktree_dir: str = "~/worktrees"

  @model_validator(mode="after")
  def _expand_tilde(self) -> "PathsConfig":
    """Expand ``~`` in both path settings against the process HOME."""
    self.workspace_dirs = [os.path.expanduser(p) for p in self.workspace_dirs]
    self.worktree_dir = os.path.expanduser(self.worktree_dir)
    return self


class BackendsConfig(BaseModel):
  """``backends:`` section: model-switch options and the selector preference order."""

  model_config = ConfigDict(extra='forbid')

  # Ordered preference list of BackendOption ids, consumed by two selectors:
  #   - checking-role (reviewer, verify default): first entry that DIFFERS from the
  #     checked party's backend and resolves — see review.select_reviewer_backend.
  #   - light one-shot (autonamer, recap): resolved entries in list order — see
  #     autonamer.iter_light_backends.
  # Empty list (default) skips the one-shot.
  preference: list[str] = []

  # Backend options available for model switching
  # Additional backends (Codex/Gemini/Kimi/Antigravity/etc.) must be configured via
  # ~/.charliebot/config.yaml -> backends.options.
  options: list[BackendOption] = []


class AccountsConfig(BaseModel):
  """``accounts:`` section: the Claude subscription pool and its compaction floors."""

  model_config = ConfigDict(extra='forbid')

  # Claude account pool: the subscription logins (each a CLAUDE_CONFIG_DIR) a
  # cc-claude entry without claude_config_dir draws from (src/core/claude_accounts.py).
  # Empty = no pool: every cc-claude entry resolves its login exactly as it did
  # before the pool existed.
  claude: list[ClaudeAccount] = []

  # Token floors for the Sonnet compaction the pool runs on Fable sessions.
  claude_compaction: ClaudeCompactionConfig = ClaudeCompactionConfig()


class VoiceConfig(BaseModel):
  """``voice:`` section: transcription engine selection."""

  model_config = ConfigDict(extra='forbid')

  # Voice transcription engine. 'sherpa' runs the CPU ONNX pipeline everywhere; 'qwen3_hf'
  # runs the official transformers Qwen3-ASR weights on NVIDIA GPUs (gpu-voice dependency
  # group + weights, provisioned by scripts/setup.sh on hosts with nvidia-smi). Engine
  # changes take effect on server restart.
  engine: Literal['sherpa', 'qwen3_hf'] = 'sherpa'

  # Model repository id for the qwen3_hf engine; switching tiers (1.7B <-> 0.6B) is a
  # one-value change.
  model_id: str = 'Qwen/Qwen3-ASR-1.7B-hf'


class CodeServerConfig(BaseModel):
  """``code_server:`` section: code-server integration."""

  model_config = ConfigDict(extra='forbid')

  # code-server integration
  bin: str | None = None
  config: str = "configs/code-server.yaml"


class UiConfig(BaseModel):
  """``ui:`` section: the backlog panel and the /home page service cards."""

  model_config = ConfigDict(extra='forbid')

  # Backlog panel
  backlog_repos: list[BacklogRepoConfig] = []

  # Home page — services this host runs, probed for reachability; default empty. Each card
  # links to the URL and the probe connects to the same host and port.
  home_services: list[HomeService] = []

  @model_validator(mode="after")
  def _expand_tilde(self) -> "UiConfig":
    """Expand ``~`` in each backlog repo path."""
    for entry in self.backlog_repos:
      entry.path = os.path.expanduser(entry.path)
    return self


class SlackConfig(BaseModel):
  """``slack:`` section: the summon entrypoint's user allow-list."""

  model_config = ConfigDict(extra='forbid')

  # Slack summon entrypoint
  allowed_user_ids: list[str] = []  # Slack user ids allowed to summon; empty = nobody


class PublishConfig(BaseModel):
  """``publish:`` section: the outbound static publish lane."""

  model_config = ConfigDict(extra='forbid')

  # Publish lane — the pair the outbound-link rewrite consumes (src/core/publish.py):
  # dir is the directory the host's 443 static lane serves, and public_base_url
  # is the base of the links readers outside the operator's devices open. Unconfigured
  # (either one) makes publish unavailable; the reply path then refuses instead of
  # falling back to a server-port link.
  dir: Path | None = None
  public_base_url: str | None = None

  @model_validator(mode="after")
  def _expand_tilde(self) -> "PublishConfig":
    """Expand ``~`` in the publish directory."""
    if self.dir is not None:
      self.dir = self.dir.expanduser()
    return self


class TelegramConfig(BaseModel):
  """``telegram:`` section: the notification target."""

  model_config = ConfigDict(extra='forbid')

  # Telegram notifications
  chat_id: str | None = None


def _alias_field_names(alias: str | AliasChoices | AliasPath | None) -> set[str]:
  """Flat string names behind an alias declaration, for known-name checks."""
  if isinstance(alias, str):
    return {alias}
  if isinstance(alias, AliasChoices):
    return set().union(*(_alias_field_names(choice) for choice in alias.choices))
  if isinstance(alias, AliasPath):
    return {str(alias.path[0])}
  return set()


class CharlieBotConfig(BaseModel):
  """CharlieBot configuration, loaded from ~/.charliebot/config.yaml.

  The mapping is sectioned: each settings group lives under its top-level
  section key (``server:``, ``paths:``, ``backends:``, ...) and every section
  model pins ``extra='forbid'``, so an unknown key — top-level or nested —
  errors naming it instead of being silently dropped (same rationale as
  :class:`ScheduledTaskConfig`). ``model_construct`` is overridden for the same
  reason: pydantic 2.12.5 drops unknown construct kwargs silently even under
  forbid. Copy-style home redirection goes through :meth:`with_home`.
  """

  model_config = ConfigDict(extra='forbid')

  # Paths — resolved per instantiation so CHARLIEBOT_HOME selects the profile
  charliebot_home: Path = Field(default_factory=charliebot_home_dir)

  # Plan registration page-height gate — absolute path of a headless-chromium-compatible
  # binary on the host running the server. The value stays host-local in config.yaml;
  # nothing in the repo hardcodes a path.
  headless_chrome_bin: str = ""

  server: ServerConfig = Field(default_factory=ServerConfig)
  paths: PathsConfig = Field(default_factory=PathsConfig)
  backends: BackendsConfig = Field(default_factory=BackendsConfig)
  accounts: AccountsConfig = Field(default_factory=AccountsConfig)
  voice: VoiceConfig = Field(default_factory=VoiceConfig)
  code_server: CodeServerConfig = Field(default_factory=CodeServerConfig)
  ui: UiConfig = Field(default_factory=UiConfig)
  slack: SlackConfig = Field(default_factory=SlackConfig)
  publish: PublishConfig = Field(default_factory=PublishConfig)
  telegram: TelegramConfig = Field(default_factory=TelegramConfig)

  @classmethod
  def model_construct(cls, _fields_set: set[str] | None = None, **values: object) -> "CharlieBotConfig":
    """``model_construct`` that rejects unknown keyword arguments by name.

    pydantic 2.12.5's ``model_construct`` silently drops kwargs that match no
    field — even with ``extra='forbid'`` — so a caller redirecting a non-field
    name gets a silently unredirected copy. Names outside the fields and their
    aliases raise :class:`TypeError` listing them; everything else delegates to
    ``super().model_construct()``.
    """
    known: set[str] = set(cls.model_fields)
    for field in cls.model_fields.values():
      known |= _alias_field_names(field.alias) | _alias_field_names(field.validation_alias)
    unknown = sorted(set(values) - known)
    if unknown:
      raise TypeError(f"{cls.__name__}.model_construct() got unexpected keyword argument(s): " + ", ".join(unknown))
    return super().model_construct(_fields_set, **values)

  def with_home(self, path: str | Path) -> "CharlieBotConfig":
    """Return a copy of this config with ``charliebot_home`` redirected to *path*.

    This is the supported copy-style redirection entry point: it expands ``~``,
    rejects a relative path (the same rule as :func:`charliebot_home_dir`), and
    returns a new instance via ``model_copy`` updating exactly ``charliebot_home``.
    Every derived Path property then resolves under the new home, nested models
    are preserved by reference, and the original instance is untouched. Instance
    fields have no setters, so in-place redirection was never possible anyway.

    Redirect through this method instead of constructing or copying by hand —
    the unknown-kwarg gates above make any miss raise at the call site.
    """
    home = Path(path).expanduser()
    if not home.is_absolute():
      raise ValueError(f"with_home() requires an absolute path or a '~' path; got {path!r}")
    return self.model_copy(update={"charliebot_home": home})

  @property
  def subprocess_buffer_limit(self) -> int:
    """Return the subprocess buffer limit in bytes."""
    return self.server.subprocess_buffer_limit_mb * 1024 * 1024

  @property
  def server_base_url(self) -> str:
    """Return the local base URL for CLI-to-server internal API calls."""
    return f"http://localhost:{self.server.port}"

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
    path = Path(self.code_server.config).expanduser()
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
  def credentials_file(self) -> Path:
    """The profile's credentials.yaml: the secrets split out of config.yaml."""
    return self.charliebot_home / "credentials.yaml"

  @property
  def config_d_dir(self) -> Path:
    return self.charliebot_home / "config.d"

  def get_backend_option(self, backend_id: str) -> BackendOption | None:
    """Look up a backend option by exact id; None when no entry matches."""
    return next((opt for opt in self.backends.options if opt.id == backend_id), None)

  def discover_repos(self) -> list[dict[str, str]]:
    """Scan paths.workspace_dirs for directories containing a .git folder."""
    repos: list[dict[str, str]] = []
    for dir_str in self.paths.workspace_dirs:
      parent = Path(dir_str)
      if not parent.is_dir():
        continue
      repos.extend(
          {
              "name": child.name,
              "path": str(child)
          } for child in sorted(parent.iterdir()) if child.is_dir() and (child / ".git").exists())
    return repos


def require_backend_option(cfg: CharlieBotConfig, backend_id: str, *, subject: str) -> BackendOption:
  """Return the configured backend option for `backend_id`; raise ValueError when none matches.

  The error names the checked surface with the caller's role as prefix:
  "<subject>backend 'x' is not in backends.options".
  """
  option = cfg.get_backend_option(backend_id)
  if option is None:
    raise ValueError(f"{subject}backend '{backend_id}' is not in backends.options")
  return option


T = TypeVar("T")


def _install_replace(current: T | None, fresh: T) -> T:
  """Drop the previous value and adopt the fresh one."""
  return fresh


class _HotReloadCache(Generic[T]):
  """One file-backed cache that reloads through a loader when the file's fingerprint moves.

  ``get(loader)`` re-runs *loader* only when the fingerprint differs from both
  the cached value's and the last failure's; the surrounding bookkeeping is the
  one state machine every hot-reload cache shares:

  - a failed reload keeps the previous value and logs one warning per error
    string per process (the key is exactly the field the line logs); the
    failed fingerprint is recorded, so the same broken corpus pays no parse
    and no line until it moves — the freshness rule the successful path
    follows, applied to failure;
  - a reload with nothing cached re-raises: with no fallback the raise is what
    surfaces the broken file, so no failed fingerprint is recorded;
  - a successful reload installs, clears the failed fingerprint, and re-arms
    the registry: a later relapse is a new onset and earns one new line.
  """

  def __init__(
      self,
      fingerprint: Callable[[], tuple[float, int]],
      event: str,
      install: Callable[[T | None, T], T],
  ) -> None:
    self._fingerprint = fingerprint
    self._event = event
    self._install = install
    self.value: T | None = None
    self._mtime: tuple[float, int] | None = None
    self.failed_mtime: tuple[float, int] | None = None
    self.seen = WarnOnceRegistry()

  def reset(self) -> None:
    """Forget the cached value and every fingerprint and warning state."""
    self.value = None
    self._mtime = None
    self.failed_mtime = None
    self.seen.clear()

  def seed(self, value: T) -> None:
    """Install *value* as if freshly loaded, stamped with the current fingerprint."""
    self.value = value
    self._mtime = self._fingerprint()

  def get(self, loader: Callable[[], T]) -> T:
    """Return the cached value, reloading through *loader* when the fingerprint moves."""
    fingerprint = self._fingerprint()
    if self.value is None or (fingerprint != self._mtime and fingerprint != self.failed_mtime):
      try:
        fresh = loader()
      except Exception as error:
        self.seen.log(log.warning, self._event, str(error), error=str(error))
        if self.value is None:
          raise
        # Only a fallback value makes the failed fingerprint meaningful: with
        # none, the raise above ends the process.
        self.failed_mtime = fingerprint
      else:
        self.value = self._install(self.value, fresh)
        self._mtime = fingerprint
        self.failed_mtime = None
        # The reported failure state ended: a later relapse is a new onset and
        # earns one new line.
        self.seen.clear()
    return self.value


def _file_fingerprint(name: str) -> tuple[float, int]:
  """The ``(mtime, size)`` reload cache key over one file in the profile home.

  Size comes from the same stat call and costs nothing extra; it catches
  mtime-preserving writes (``cp -p``, ``touch -r``, two writes inside one second
  on a coarse-resolution filesystem) that an mtime-only key would miss silently.
  A content change that preserves both mtime and size is deliberately not
  covered. A missing file stats to a sentinel rather than raising.

  This is the per-request path (the auth middleware's ``get_config``), so the
  stat stays on raw strings and ``os`` calls: per-call ``Path`` allocation and
  ``resolve`` measured ~130 µs of the ~150 µs middleware floor on the live
  corpus, against ~10 µs of unavoidable fresh stats.
  """
  try:
    st = os.stat(os.path.join(_resolve_home()[1], name))
  except OSError:
    return (0.0, 0)
  return (st.st_mtime, st.st_size)


def _config_fingerprint() -> tuple[float, int]:
  """The reload cache key over ``config.yaml``: :func:`_file_fingerprint` on it."""
  return _file_fingerprint("config.yaml")


def _install_config_snapshot(current: CharlieBotConfig | None, fresh: CharlieBotConfig) -> CharlieBotConfig:
  """First install adopts *fresh*; a reload copies field-by-field into the held instance.

  Assignment validation is off, so the source must already be a fully
  validated CharlieBotConfig.
  """
  if current is None:
    return fresh
  for name in type(fresh).model_fields:
    setattr(current, name, getattr(fresh, name))
  return current


_config_cache = _HotReloadCache(
    fingerprint=_config_fingerprint, event="config_reload_failed", install=_install_config_snapshot)

# Retired config.yaml top-level keys: the loader rejects any file still carrying
# one, and the error names where the key moved. A plain dotted value points into
# the sectioned mapping; a value under :data:`CREDENTIALS_PREFIX` moves into
# credentials.yaml (secrets live there, and the suffix is that file's key path);
# a ``removed...`` value has no successor.
CREDENTIALS_PREFIX = "credentials: "

LEGACY_KEYS: dict[str, str] = {
    "server_host": "server.host",
    "server_port": "server.port",
    "subprocess_buffer_limit_mb": "server.subprocess_buffer_limit_mb",
    "workspace_dirs": "paths.workspace_dirs",
    "project_dirs": "paths.workspace_dirs",
    "worktree_dir": "paths.worktree_dir",
    # Per-entry moves inside a backend option: claude_config_dir and codex_home are
    # retired, api_key/api_key_env fold into credential, opencode_proxy_url into
    # proxy_url, aliases retired.
    "backend_options": "backends.options",
    "model_preference": "backends.preference",
    "claude_accounts": "accounts.claude",
    "claude_compaction": "accounts.claude_compaction",
    "voice_engine": "voice.engine",
    "voice_model_id": "voice.model_id",
    "code_server_bin": "code_server.bin",
    "code_server_config": "code_server.config",
    "backlog_repos": "ui.backlog_repos",
    "home_services": "ui.home_services",
    "backlog_repo": "removed (list the repo under ui.backlog_repos)",
    "backlog_label": "removed",
    "slack_allowed_user_ids": "slack.allowed_user_ids",
    "publish_dir": "publish.dir",
    "public_base_url": "publish.public_base_url",
    "telegram_chat_id": "telegram.chat_id",
    CREDENTIALS_PREFIX + "slack_bot_token": "slack.bot_token",
    CREDENTIALS_PREFIX + "slack_app_token": "slack.app_token",
    CREDENTIALS_PREFIX + "slack_user_token": "slack.user_token",
    CREDENTIALS_PREFIX + "telegram_bot_token": "telegram.bot_token",
    CREDENTIALS_PREFIX + "charliebot_access_key": "charliebot.access_key",
    CREDENTIALS_PREFIX + "moonshot_api_key": "moonshot.api_key",
    CREDENTIALS_PREFIX + "aigw_api_key": "aigw.api_key",
    CREDENTIALS_PREFIX + "linear_api_key": "linear.api_key",
    CREDENTIALS_PREFIX + "gemini_api_key": "gemini.api_key",
    CREDENTIALS_PREFIX + "gemini_model": "gemini.model",
    CREDENTIALS_PREFIX + "feishu_app_id": "feishu.app_id",
    CREDENTIALS_PREFIX + "feishu_app_secret": "feishu.app_secret",
    CREDENTIALS_PREFIX + "feishu_refresh_token": "feishu.refresh_token",
    CREDENTIALS_PREFIX + "feishu_user_access_token": "feishu.user_access_token",
    CREDENTIALS_PREFIX + "google_client_id": "google.client_id",
    CREDENTIALS_PREFIX + "google_client_secret": "google.client_secret",
    CREDENTIALS_PREFIX + "google_refresh_token": "google.refresh_token",
    CREDENTIALS_PREFIX + "google_docs_client_id": "google.client_id",
    CREDENTIALS_PREFIX + "google_docs_client_secret": "google.client_secret",
    CREDENTIALS_PREFIX + "google_docs_refresh_token": "google.refresh_token",
    CREDENTIALS_PREFIX + "google_docs_default_folder_id": "google.docs_default_folder_id",
    CREDENTIALS_PREFIX + "twitter_api_key": "twitter.api_key",
    CREDENTIALS_PREFIX + "twitter_api_secret": "twitter.api_secret",
    CREDENTIALS_PREFIX + "twitter_access_token": "twitter.access_token",
    CREDENTIALS_PREFIX + "twitter_access_token_secret": "twitter.access_token_secret",
}


def load_config() -> CharlieBotConfig:
  """Load config from this profile's ``config.yaml``.

  The file holds the whole sectioned mapping; secrets live separately in
  ``credentials.yaml``. Two tripwires fire before validation: any ``*.yaml``
  file directly under ``config.d/`` (only ``config.d/cron.d/`` holds fragment
  files now), and any top-level key from :data:`LEGACY_KEYS` — structure keys
  and secrets alike; the error opens with the config path and names each old
  key with its new location (a secret's location is its ``credentials.yaml``
  key path).
  """
  home = charliebot_home_dir()
  config_path = home / "config.yaml"

  config_d = home / "config.d"
  if config_d.is_dir():
    for entry in sorted(config_d.iterdir()):
      if entry.name.endswith(".yaml") and entry.is_file():
        raise ValueError(
            f"{entry} is not a config location: only config.d/cron.d/ holds fragment files; "
            "keys belong in config.yaml (structure) or credentials.yaml (secrets)")

  yaml_data: dict = load_yaml(config_path, default={})
  legacy_hits = [key for key in yaml_data if key in LEGACY_KEYS or CREDENTIALS_PREFIX + key in LEGACY_KEYS]
  if legacy_hits:
    lines = "\n".join(
        f"  {key} -> " +
        (LEGACY_KEYS[key] if key in LEGACY_KEYS else "credentials.yaml " + LEGACY_KEYS[CREDENTIALS_PREFIX + key])
        for key in legacy_hits)
    raise ValueError(f"{config_path} still uses retired top-level keys; move each one:\n{lines}")

  # The home directory is chosen by the environment, never by a file that lives
  # inside it: honouring the key would leave the config loaded from one profile and
  # the state written to another, and dropping it silently would hide the mistake.
  if "charliebot_home" in yaml_data:
    raise ValueError(
        f"{config_path} sets 'charliebot_home'; that path is chosen by the "
        f"{CHARLIEBOT_HOME_ENV} environment variable. Remove the key.")
  try:
    return CharlieBotConfig(charliebot_home=home, **yaml_data)
  except ValidationError as e:
    # An unknown field inside a backend entry gets its own message: the raw
    # entry's id and type are what the operator greps the file for. The
    # discriminated union inserts the matched type tag into the error path, so
    # the field name is the last segment.
    for err in e.errors():
      if err["type"] == "extra_forbidden" and err["loc"][:2] == ("backends", "options"):
        raw_entry = yaml_data["backends"]["options"][err["loc"][2]]
        raise ValueError(
            f"backend entry '{raw_entry.get('id')}' (type {raw_entry.get('type')}) "
            f"has unknown field '{err['loc'][-1]}'") from e
    extras = [err["loc"][0] for err in e.errors() if err["type"] == "extra_forbidden" and len(err["loc"]) == 1]
    if not extras:
      raise
    raise ValueError(
        "unknown config key(s) " + ", ".join(repr(key) for key in extras) +
        "; declare the key(s) on CharlieBotConfig or remove them") from e


def require_backends(cfg: CharlieBotConfig) -> None:
  """Raise ValueError when ``backends.options`` is empty.

  The server calls this once at startup because every session and cron run
  resolves a backend from this list, so an empty list is a deployment error
  worth stopping on. ``load_config`` stays permissive for CLIs that never
  resolve a backend.
  """
  if not cfg.backends.options:
    raise ValueError(
        "config.yaml: backends.options lists no backend; "
        "copy the starter entries from configs/config.example.yaml")


def get_config() -> CharlieBotConfig:
  """Return the process-wide config, refreshed in place when ``config.yaml`` changes.

  The reload key is ``config.yaml``'s ``(mtime, size)`` — see
  :func:`_config_fingerprint`. The returned instance keeps a stable identity
  across reloads: holders that captured it earlier (manager singletons,
  in-flight coroutines) observe the new values without re-fetching. Replacing
  the object instead would leave every such holder pinned to a stale snapshot.
  """
  return _config_cache.get(load_config)


@dataclass
class Credentials:
  """One profile's ``credentials.yaml``: ``section -> key -> scalar`` secret values.

  Deliberately outside :class:`CharlieBotConfig`: the structure file never
  carries secrets, so nothing holding a config can leak one. :meth:`get`
  answers "is it set"; :meth:`require` turns a missing value into a
  :class:`ValueError` naming the key path and the file it is missing from.
  """

  path: Path
  sections: dict[str, dict[str, str | int]]

  def get(self, section: str, key: str) -> str | int | None:
    """Return the value under *section*/*key*, or None when it is unset."""
    return self.sections.get(section, {}).get(key)

  def require(self, section: str, key: str) -> str | int:
    """Return the value under *section*/*key*, raising :class:`ValueError` when it is unset."""
    value = self.get(section, key)
    if value is None:
      raise ValueError(f"credentials.{section}.{key} is not set in {self.path}")
    return value


def load_credentials() -> Credentials:
  """Load this profile's ``credentials.yaml``, the secrets file split out of ``config.yaml``.

  A missing file loads as empty sections. The document must be a mapping whose
  values are mappings whose values are strings or integers; a ``None`` value
  counts as unset and is dropped. Any other shape raises :class:`ValueError`
  naming the offending path as ``credentials.<section>`` or
  ``credentials.<section>.<key>``. Section and key names are never validated:
  any name loads.
  """
  path = charliebot_home_dir() / "credentials.yaml"
  data = load_yaml(path, default={})
  if data is None:
    data = {}
  if not isinstance(data, dict):
    raise ValueError(f"credentials must be a mapping of sections: {path}")
  sections: dict[str, dict[str, str | int]] = {}
  for section, keys in data.items():
    if not isinstance(keys, dict):
      raise ValueError(f"credentials.{section} must be a mapping of keys: {path}")
    entry: dict[str, str | int] = {}
    for key, value in keys.items():
      if value is None:
        continue
      if not isinstance(value, (str, int)):
        raise ValueError(f"credentials.{section}.{key} must be a string or integer: {path}")
      entry[key] = value
    sections[section] = entry
  return Credentials(path=path, sections=sections)


def _credentials_fingerprint() -> tuple[float, int]:
  """The reload cache key over ``credentials.yaml``: :func:`_file_fingerprint` on it."""
  return _file_fingerprint("credentials.yaml")


_credentials_cache = _HotReloadCache(
    fingerprint=_credentials_fingerprint, event="credentials_reload_failed", install=_install_replace)


def get_credentials() -> Credentials:
  """Return the process-wide credentials, refreshed when ``credentials.yaml`` changes.

  Independent of :func:`get_config`: the reload key is ``credentials.yaml``'s
  ``(mtime, size)`` (see :func:`_credentials_fingerprint`), and the cached
  :class:`Credentials` is replaced wholesale — its consumers read per call and
  hold no instance references. A failed reload keeps the previous value and
  logs one warning per onset; with nothing cached yet the error propagates.
  """
  return _credentials_cache.get(load_credentials)


_cron_snapshot = _CronSnapshot()


def _resolve_pointer_path(pointer: str, repo: Path) -> Path:
  """Resolve one raw ``prompt_file`` pointer: ``~``-prefixed or absolute literal, else against *repo*."""
  if pointer.startswith("~") or Path(pointer).is_absolute():
    return Path(os.path.expanduser(pointer))
  return repo / pointer


def _resolve_prompt_file(entry: dict, repo_root: Path) -> Path | None:
  """Resolve a cron entry's ``prompt_file`` into ``prompt`` in place.

  Reads the referenced file, sets ``entry['prompt']`` to its contents, and
  removes the ``prompt_file`` key while resolving. Callers that expose the
  runtime model restore the raw pointer after this step. Returns the resolved
  :class:`Path` (for mtime tracking) or ``None`` if the entry had no
  ``prompt_file``.

  Path resolution has no search order and no shadowing:
  :func:`_resolve_pointer_path` is the rule.

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
  path = _resolve_pointer_path(pf, repo_root)
  try:
    body = path.read_text(encoding="utf-8")
  except OSError as e:
    raise ValueError(f"cron entry {entry.get('name')!r} prompt_file unreadable: {path} ({e})") from e
  entry["prompt"] = body
  del entry["prompt_file"]
  return path


# Claude Code's login-directory env var, a cross-process wire contract: the server
# writes it onto a cc-claude child (claude_code._prepare_env, the tmux spawn in
# src/cli/claude_sub.py), the pool strips any inherited value where it pinned the
# directory itself (master_cc_run, claude_compaction.compaction_env), and the
# in-process readers below and in tui/_claude_config_path and claude_sub read it
# back. One spelling everywhere.
CLAUDE_CONFIG_DIR_ENV_VAR = "CLAUDE_CONFIG_DIR"


def claude_config_dir(account: ClaudeAccount | None = None) -> Path:
  """Resolve the CLAUDE_CONFIG_DIR a cc-claude process will use.

  Single source of truth for the resolution order: the pool account's
  ``config_dir`` when one is pinned, then ``$CLAUDE_CONFIG_DIR``, then
  ``~/.claude``. Both the API backend-switch guard and the runtime resume
  resolver call this — do not restate the order anywhere else.
  """
  if account is not None:
    return Path(account.config_dir).expanduser()
  env_dir = os.environ.get(CLAUDE_CONFIG_DIR_ENV_VAR)
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
      current[p] = os.stat(str(p)).st_mtime
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


def _prompt_pointer_entries(body: dict) -> list[dict]:
  """The mapping entries a cron prompt-pointer walk covers: the body, then each mapping step."""
  return [body] + [step for step in body.get("steps") or [] if isinstance(step, dict)]


def _record_prompt_mtime(prompt_mtimes: dict[Path, float], path: Path) -> None:
  """Record *path*'s mtime in *prompt_mtimes*, or the 0.0 sentinel when it has vanished.

  The sentinel keeps a vanished pointer file in the hot-reload fingerprint so
  the next tick re-reads it instead of serving a cached body. Only
  :class:`OSError` rides the sentinel: the fingerprint walker
  :func:`_stat_prompt_files` catches no other stat failure, so a recorded path
  that raises, e.g. :class:`ValueError` on an embedded null byte, would escape
  :func:`get_scheduled_tasks` and break its never-raises contract on every
  later tick.
  """
  try:
    prompt_mtimes[path] = path.stat().st_mtime
  except OSError:
    prompt_mtimes[path] = 0.0


def _resolve_prompt_pointer(entry: dict, repo: Path, prompt_mtimes: dict[Path, float]) -> None:
  """Resolve one mapping's ``prompt_file`` into ``prompt`` in place.

  Restores the raw pointer on the mapping afterwards (the pointer is what the
  API and UI display) and records the resolved file's mtime into
  *prompt_mtimes* for the hot-reload fingerprint. Applies to a task body and
  to each ``steps`` entry alike; a mapping without a pointer is a no-op.
  """
  prompt_file = entry.get("prompt_file")
  if not prompt_file:
    return
  resolved = _resolve_prompt_file(entry, repo)
  entry["prompt_file"] = prompt_file  # preserve the raw pointer for the API/UI
  _record_prompt_mtime(prompt_mtimes, resolved)


def _validate_cron_body(body: dict, repo: Path, stem: str) -> tuple[ScheduledTaskConfig, dict[Path, float]]:
  """Resolve and validate one cron job body into a ``ScheduledTaskConfig``.

  Mutates *body* in place — resolves ``prompt_file`` (setting ``prompt`` to the
  referenced file's body and preserving the raw pointer on the model), resolves
  each ``steps`` entry's ``prompt_file`` the same way, rewrites a literal
  ``timezone: local`` to the host IANA zone, and expands ``~`` in
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
  for entry in _prompt_pointer_entries(body):
    _resolve_prompt_pointer(entry, repo, prompt_mtimes)
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
  for step in body.get("steps") or []:
    if isinstance(step, dict) and "prompt" in step:
      raise ValueError(
          "a cron.d host file must not carry an inline 'prompt'; a step holds the "
          "path to its prompt source under 'prompt_file', and the loader reads "
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
        errors.append(
            ScheduledTaskError(
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
          if isinstance(failed_body, dict):
            for entry in _prompt_pointer_entries(failed_body):
              pointer = entry.get("prompt_file")
              if isinstance(pointer, str) and pointer:
                try:
                  _record_prompt_mtime(prompt_mtimes, _resolve_pointer_path(pointer, repo))
                except ValueError as stat_error:
                  # An unstatable pointer (e.g. an embedded null byte) must stay out of the
                  # fingerprint; see _record_prompt_mtime. Skipping it keeps this loader total.
                  log.debug("cron_failed_prompt_path_unstatable", path=str(pointer), error=str(stat_error))
        errors.append(
            ScheduledTaskError(name=stem, path=str(path), error=str(e), enabled=_read_cron_file_enabled(path)))
        log.error("cron_task_load_failed", name=stem, path=str(path), error=str(e))
        continue
      tasks.append(task)
      prompt_mtimes.update(file_prompt_mtimes)

  # Legacy tripwire: a leftover config.d/cron.yaml is a loud error, never a
  # silent fallback. None of its entries are loaded.
  if legacy_file.exists():
    errors.append(
        ScheduledTaskError(
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
  fires one notification — ``"⚠️ cron tasks failed to load: <names>"`` when the new set
  is non-empty, ``"✅ all cron load failures resolved"`` when it turned empty (recovery
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
    message = "⚠️ cron tasks failed to load: " + ", ".join(names)
  else:
    message = "✅ all cron load failures resolved"
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


def _cron_fingerprint(
    prompt_mtimes: dict[Path, float],) -> tuple[tuple[tuple[str, float], ...], dict[Path, float] | None, bool]:
  """Compute the hot-reload fingerprint over all four re-read inputs.

  The set of ``cron.d/*.yaml`` paths with each file's mtime, the mtime of every
  referenced ``prompt_file`` (a referenced file that has gone missing makes the
  stat fail, returning ``None`` and forcing a full re-read so the failure
  surfaces instead of a stale cached body), and whether the legacy
  ``config.d/cron.yaml`` exists.

  This walk runs on every ``get_scheduled_tasks`` call (each /scheduled and
  /api/cron/tasks request, every scheduler tick), so it is one ``os.scandir``
  over the raw string dir with ``DirEntry`` answering
  ``is_file`` from the directory record, no per-entry ``Path`` construction —
  the pathlib form measured 174 us vs 53 us on the live 13-file corpus.
  """
  cron_d = str(cron_dir())
  legacy_file = str(_legacy_cron_file())
  files: list[tuple[str, float]] = []
  if os.path.isdir(cron_d):
    with os.scandir(cron_d) as entries:
      for entry in sorted(entries, key=lambda e: e.name):
        try:
          if not entry.is_file():
            continue
        except OSError:
          continue  # the pathlib is_file contract: unreadable entry reads as absent
        if entry.name.endswith(".yaml") and not entry.name.startswith("."):
          try:
            files.append((entry.name, entry.stat().st_mtime))
          except OSError:
            files.append((entry.name, 0.0))
  current_prompt_mtimes = _stat_prompt_files(prompt_mtimes)
  return (tuple(files), current_prompt_mtimes, os.path.exists(legacy_file))


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
