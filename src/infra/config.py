"""Configuration loading for CharlieBot."""

import os
from pathlib import Path
from typing import Annotated, Any, TypeVar

from pydantic import (
    AliasChoices,
    AliasPath,
    BaseModel,
    BeforeValidator,
    ConfigDict,
    Field,
    SerializeAsAny,
    ValidationError,
    model_validator,
)

from src.infra import config_registry, home
from src.infra.backend_models import BackendOption, parse_option
from src.infra.config_registry import CREDENTIALS_PREFIX
from src.infra.constants import REPO_ROOT
from src.infra.credentials import (  # noqa: F401  (re-export: the established src.infra.config import path)
    CREDENTIALS_FILENAME,
    Credentials,
    _credentials_cache,
    # Not facade surface: nothing reaches this name through src.infra.config — its call
    # sites go through src.infra.credentials, and it serves this module's own _config_cache.
    _HotReloadCache,
    configured_access_key,
    get_credentials,
    load_credentials,
)
from src.infra.home import (  # noqa: F401  (re-export: the established src.infra.config import path)
    CHARLIEBOT_HOME_ENV,
    CLAUDE_CONFIG_DIR_ENV_VAR,
    charliebot_home_dir,
    default_charliebot_home,
    default_claude_dir,
)
from src.infra.log_once import LazyStructlogLogger
from src.infra.yaml_utils import load_yaml

log = LazyStructlogLogger()

# Fixed house wall clock pinned by Slack timestamp prefixes
# (src/features/slack/slack_listener.py) and the Saturday-1AM weekly-recycle anchor
# (src/runtime/master_trigger.py). Distinct from DEFAULT_TIMEZONE below, a per-task default
# overridable via ``timezone: local`` or any IANA key, so retargeting the cron default
# cannot shift these pins.
HOUSE_TIMEZONE = "America/Los_Angeles"

# The profile's config filename, named once: the loader, the reload fingerprint,
# and ``config_file`` must resolve to the same file, and the preview-home setup
# (src/features/session_tree_preview/session_tree_preview.py) writes it by that name. A rename that missed
# one site would leave that site silently reading a different file.
CONFIG_FILENAME = "config.yaml"


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

  # Per-session memory-cap cgroup, MB. Every agent process a
  # session spawns (master, workers, one-shots, compaction) is forked into the
  # session's cgroup and held to these hard limits; on a limit breach the
  # kernel kills only the cgroup's largest process. 0 disables cgroup control
  # entirely. session_swap_max_mb bounds swap use separately (0 = no swap).
  session_memory_max_mb: int = 12288
  session_swap_max_mb: int = 0

  # Worker-class launch precheck: the filesystems holding the CharlieBot data
  # dir (~/.charliebot) and the worktree root must hold at least this much
  # free space, or the run stays queued with a blocked report to its parent
  # (an environment install is the write-heavy case). 0 disables the check.
  # Manager turns never check: they write little and are the path that tells
  # the operator about the shortage.
  min_free_disk_gib: int = 10


class PathsConfig(BaseModel):
  """``paths:`` section: repos to scan and where worker worktrees live."""

  model_config = ConfigDict(extra='forbid')

  # Workspace directories to scan for git repos
  workspace_dirs: list[str] = ["~/workspace"]

  # Root directory for worker worktrees
  worktree_dir: str = "~/worktrees"

  @model_validator(mode="after")
  def _expand_tilde(self) -> PathsConfig:
    """Expand ``~`` in both path settings against the process HOME."""
    self.workspace_dirs = [os.path.expanduser(p) for p in self.workspace_dirs]
    self.worktree_dir = os.path.expanduser(self.worktree_dir)
    return self


class BackendsConfig(BaseModel):
  """``backends:`` section: model-switch options and the selector preference order."""

  model_config = ConfigDict(extra='forbid')

  # Backend id rule (the one home of it; other docs point here). An option id is
  # "<type prefix>-<family>" (claude-opus, codex-luna, charlie-code-kimi), the
  # prefix one of claude-, codex-, charlie-code-. The id names the model family
  # and carries no version: the version lives only in the entry's `model` and
  # `label`. Session, thread and Run metadata, package config files and `preference` store
  # the id, so a version bump edits exactly those two fields of one entry and
  # every stored reference stays valid. The usage tally classifies a retired id
  # (off config, still in old records) by its prefix. An option bound to an
  # account pool may end its id with `-<pool name>`. require_backends refuses a
  # startup whose preference names an id missing from `options`; a package's startup
  # check does the same for the ids in that package's config files.

  # Ordered preference list of BackendOption ids, consumed by two selectors:
  #   - checking-role (reviewer, verify default): first entry that DIFFERS from the
  #     checked party's backend and resolves — see review.select_reviewer_backend.
  #   - light one-shot (autonamer, recap): resolved entries in list order — see
  #     autonamer.iter_light_backends.
  # Empty list (default) skips the one-shot.
  preference: list[str] = []

  # Backend options available for model switching. Each entry parses as the option model that
  # its ``type`` names in the config registry (src/infra/config_registry.py). Additional
  # backends must be configured via ~/.charliebot/config.yaml -> backends.options.
  options: list[Annotated[SerializeAsAny[BackendOption], BeforeValidator(parse_option)]] = []


class UiConfig(BaseModel):
  """``ui:`` section: the /home page service cards."""

  model_config = ConfigDict(extra='forbid')

  # Home page — services this host runs, probed for reachability; default empty. Each card
  # links to the URL and the probe connects to the same host and port.
  home_services: list[HomeService] = []


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


def _locate_in_section(key: str, err: Any) -> Any:
  """One pydantic error detail of section *key*'s own validation, with *key* as the first segment of its location."""
  located = {"type": err["type"], "loc": (key, *err["loc"]), "input": err["input"]}
  if "ctx" in err:
    located["ctx"] = err["ctx"]
  return located


class CharlieBotConfig(BaseModel):
  """CharlieBot configuration, loaded from ~/.charliebot/config.yaml.

  The mapping is sectioned: each settings group lives under its top-level
  section key (``server:``, ``paths:``, ``backends:``, ...) and every section
  model pins ``extra='forbid'``, so an unknown key — top-level or nested —
  errors naming it instead of being silently dropped (the same rationale as the
  package models that reject unknown keys). ``model_construct`` is overridden for the same
  reason: pydantic 2.12.5 drops unknown construct kwargs silently even under
  forbid.

  A package owns a section by registering it in :mod:`src.infra.config_registry`.
  The parsed section is a pydantic extra, so ``cfg.<key>`` reads it like a field,
  and ``_parse_package_sections`` refuses every key that is neither a field nor a
  registered section.
  """

  model_config = ConfigDict(extra='allow')

  # Paths — resolved per instantiation so CHARLIEBOT_HOME selects the profile
  charliebot_home: Path = Field(default_factory=charliebot_home_dir)

  # Plan registration page-height gate — absolute path of a headless-chromium-compatible
  # binary on the host running the server. The value stays host-local in config.yaml;
  # nothing in the repo hardcodes a path.
  headless_chrome_bin: str = ""

  server: ServerConfig = Field(default_factory=ServerConfig)
  paths: PathsConfig = Field(default_factory=PathsConfig)
  backends: BackendsConfig = Field(default_factory=BackendsConfig)
  ui: UiConfig = Field(default_factory=UiConfig)
  telegram: TelegramConfig = Field(default_factory=TelegramConfig)

  @classmethod
  def _field_names(cls) -> set[str]:
    """The top-level names the model declares: its fields and their aliases."""
    names = set(cls.model_fields)
    for field in cls.model_fields.values():
      names |= _alias_field_names(field.alias) | _alias_field_names(field.validation_alias)
    return names

  @model_validator(mode='before')
  @classmethod
  def _parse_package_sections(cls, data: Any) -> Any:
    """Parse the registered sections and refuse every key that no field or section names.

    A section the input omits takes the model's defaults. Each error keeps the section
    key as the first segment of its location, as a declared field's error would.
    """
    if not isinstance(data, dict):
      return data
    sections = config_registry.section_models()
    known = cls._field_names() | set(sections)
    unknown = [key for key in data if key not in known]
    if unknown:
      raise ValidationError.from_exception_data(
          cls.__name__, [{
              "type": "extra_forbidden",
              "loc": (key,),
              "input": data[key]
          } for key in unknown])
    parsed = dict(data)
    for key, model in sections.items():
      try:
        parsed[key] = model.model_validate(data[key]) if key in data else model()
      except ValidationError as exc:
        raise ValidationError.from_exception_data(
            cls.__name__, [_locate_in_section(key, err) for err in exc.errors(include_url=False)]) from exc
    return parsed

  @model_validator(mode='after')
  def _run_package_checks(self) -> CharlieBotConfig:
    """Run the checks that packages registered, once the whole config has validated."""
    config_registry.run_config_checks(self)
    return self

  @classmethod
  def model_construct(cls, _fields_set: set[str] | None = None, **values: object) -> CharlieBotConfig:
    """``model_construct`` that rejects unknown keyword arguments by name.

    pydantic 2.12.5's ``model_construct`` silently drops kwargs that match no
    field — even with ``extra='forbid'`` — so a caller redirecting a non-field
    name gets a silently unredirected copy. Names outside the fields, their
    aliases and the registered sections raise :class:`TypeError` listing them;
    everything else delegates to ``super().model_construct()``, with each
    registered section the caller omits set to its defaults, as validation does.
    """
    sections = config_registry.section_models()
    unknown = sorted(set(values) - cls._field_names() - sections.keys())
    if unknown:
      raise TypeError(f"{cls.__name__}.model_construct() got unexpected keyword argument(s): " + ", ".join(unknown))
    for key, model in sections.items():
      values.setdefault(key, model())
    return super().model_construct(_fields_set, **values)

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
    return REPO_ROOT

  @property
  def config_file(self) -> Path:
    return self.charliebot_home / CONFIG_FILENAME

  @property
  def credentials_file(self) -> Path:
    """The profile's credentials.yaml: the secrets split out of config.yaml."""
    return self.charliebot_home / CREDENTIALS_FILENAME

  @property
  def config_d_dir(self) -> Path:
    return self.charliebot_home / "config.d"

  def get_backend_option(self, backend_id: str) -> BackendOption | None:
    """Look up a backend option by exact id; None when no entry matches."""
    return next((opt for opt in self.backends.options if opt.id == backend_id), None)

  def discover_repos(self) -> list[dict[str, str]]:
    """Scan paths.workspace_dirs (one level deep) for directories containing a .git folder.

    Returns {"name", "path"} entries with resolved absolute paths, deduplicated
    by path and sorted by name; the endpoint adapters only rename the name key.
    """
    found: dict[str, dict[str, str]] = {}
    for dir_str in self.paths.workspace_dirs:
      parent = Path(dir_str)
      if not parent.is_dir():
        continue
      for child in parent.iterdir():
        if not child.is_dir() or not (child / ".git").exists():
          continue
        path = str(child.resolve())
        found.setdefault(path, {"name": child.name, "path": path})
    return sorted(found.values(), key=lambda repo: repo["name"])


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


def _config_fingerprint() -> tuple[float, int]:
  """The reload cache key over ``config.yaml``: :func:`src.infra.home.file_fingerprint` on it."""
  return home.file_fingerprint(CONFIG_FILENAME)


def _install_config_snapshot(current: CharlieBotConfig | None, fresh: CharlieBotConfig) -> CharlieBotConfig:
  """First install adopts *fresh*; a reload copies field-by-field into the held instance.

  Assignment validation is off, so the source must already be a fully
  validated CharlieBotConfig.
  """
  if current is None:
    return fresh
  for name in type(fresh).model_fields:
    setattr(current, name, getattr(fresh, name))
  for name, section in fresh.__pydantic_extra__.items():
    setattr(current, name, section)
  return current


_config_cache = _HotReloadCache(
    fingerprint=_config_fingerprint, event="config_reload_failed", install=_install_config_snapshot)

# Retired config.yaml top-level keys: the loader rejects any file still carrying
# one, and the error names where the key moved. A plain dotted value points into
# the sectioned mapping; a value under :data:`CREDENTIALS_PREFIX` moves into
# credentials.yaml (secrets live there, and the suffix is that file's key path);
# a ``removed...`` value has no successor. A package's own retired keys register
# through :mod:`src.infra.config_registry`; the loader reads both maps.
LEGACY_KEYS: dict[str, str] = {
    "server_host": "server.host",
    "server_port": "server.port",
    "subprocess_buffer_limit_mb": "server.subprocess_buffer_limit_mb",
    "workspace_dirs": "paths.workspace_dirs",
    "project_dirs": "paths.workspace_dirs",
    "worktree_dir": "paths.worktree_dir",
    "backend_options": "backends.options",
    "model_preference": "backends.preference",
    "home_services": "ui.home_services",
    "telegram_chat_id": "telegram.chat_id",
    CREDENTIALS_PREFIX + "slack_bot_token": "slack.bot_token",
    CREDENTIALS_PREFIX + "slack_app_token": "slack.app_token",
    CREDENTIALS_PREFIX + "slack_user_token": "slack.user_token",
    CREDENTIALS_PREFIX + "telegram_bot_token": "telegram.bot_token",
    CREDENTIALS_PREFIX + "charliebot_access_key": "charliebot.access_key",
    CREDENTIALS_PREFIX + "moonshot_api_key": "moonshot.api_key",
    CREDENTIALS_PREFIX + "aigw_api_key": "aigw.api_key",
    CREDENTIALS_PREFIX + "linear_api_key": "linear.api_key",
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
  file directly under ``config.d/`` (package fragment files live in its
  subdirectories), and any top-level key from :data:`LEGACY_KEYS` or a package's
  registered legacy keys — structure keys and secrets alike; the error opens
  with the config path and names each old key with its new location (a
  secret's location is its ``credentials.yaml`` key path).
  """
  home = charliebot_home_dir()
  config_path = home / CONFIG_FILENAME

  config_d = home / "config.d"
  if config_d.is_dir():
    for entry in sorted(config_d.iterdir()):
      if entry.name.endswith(".yaml") and entry.is_file():
        raise ValueError(
            f"{entry} is not a config location: package fragment files live in config.d/ subdirectories; "
            "keys belong in config.yaml (structure) or credentials.yaml (secrets)")

  yaml_data: dict = load_yaml(config_path, default={})
  legacy_keys = {**LEGACY_KEYS, **config_registry.legacy_keys()}
  legacy_hits = [key for key in yaml_data if key in legacy_keys or CREDENTIALS_PREFIX + key in legacy_keys]
  if legacy_hits:
    lines = "\n".join(
        f"  {key} -> " +
        (legacy_keys[key] if key in legacy_keys else "credentials.yaml " + legacy_keys[CREDENTIALS_PREFIX + key])
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
    # entry's id and type are what the operator greps the file for. The error
    # path is ("backends", "options", index, field), so the field name is the
    # last segment.
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
        "; declare the key(s) on CharlieBotConfig, register a config section for them, or remove them") from e


def require_backends(cfg: CharlieBotConfig) -> None:
  """Raise ValueError when ``backends.options`` or ``backends.preference`` is broken.

  The server calls this once at start, because every session and every package-started
  run resolves a backend id against ``backends.options``; a broken catalog is a
  deployment error worth stopping on. Refused: an empty list, an option id
  listed twice, and a ``backends.preference`` entry naming no option id. One
  error lists every violation, each line naming the file, the entry and the id.
  Each package's startup check covers that package's own references into the
  catalog. ``load_config`` stays permissive for CLIs that never resolve a backend.
  """
  if not cfg.backends.options:
    raise ValueError(
        "config.yaml: backends.options lists no backend; "
        "copy the starter entries from configs/config.example.yaml")
  problems: list[str] = []
  ids: set[str] = set()
  for index, option in enumerate(cfg.backends.options):
    if option.id in ids:
      problems.append(f"{cfg.config_file}: backends.options[{index}] repeats id '{option.id}'")
    ids.add(option.id)
  for index, backend_id in enumerate(cfg.backends.preference):
    if backend_id not in ids:
      problems.append(f"{cfg.config_file}: backends.preference[{index}] names unknown backend '{backend_id}'")
  if problems:
    raise ValueError(
        "backend references must name a backends.options id (id rule: BackendsConfig in "
        "src/infra/config.py):\n" + "\n".join(f"  {problem}" for problem in problems))


def get_config() -> CharlieBotConfig:
  """Return the process-wide config, refreshed in place when ``config.yaml`` changes.

  The reload key is ``config.yaml``'s ``(mtime, size)`` — see
  :func:`_config_fingerprint`. The returned instance keeps a stable identity
  across reloads: holders that captured it earlier (manager singletons,
  in-flight coroutines) observe the new values without re-fetching. Replacing
  the object instead would leave every such holder pinned to a stale snapshot.
  """
  return _config_cache.get(load_config)


# The CLAUDE_CONFIG_DIR cross-process wire contract (writers, pool strips,
# readers) is stated once, on CLAUDE_CONFIG_DIR_ENV_VAR in src.infra.home.


def claude_config_dir() -> Path:
  """Resolve the CLAUDE_CONFIG_DIR a cc-claude process will use.

  Single source of truth for the resolution order: ``$CLAUDE_CONFIG_DIR``
  first, then ``~/.claude``. Both the API backend-switch guard and the
  runtime resume resolver call this — do not restate the order anywhere
  else. A pool account's pinned ``config_dir`` rides the ``CLAUDE_CONFIG_DIR``
  value the backend sets on the process environment, never this call.
  """
  env_dir = os.environ.get(CLAUDE_CONFIG_DIR_ENV_VAR)
  if env_dir:
    return Path(env_dir).expanduser()
  return default_claude_dir()
