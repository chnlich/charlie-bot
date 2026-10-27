"""Configuration loading for CharlieBot."""

import os
import pathlib
from typing import Annotated, Any, TypeVar

import pydantic

from src.infra import backend_models, config_registry, constants, credentials, home, log_once, yaml_utils

# Re-export: ``config.<name>`` is the established read path for these
# credentials and home names across the app and the tests; the aliases keep it
# while the import above names modules only.
CREDENTIALS_FILENAME = credentials.CREDENTIALS_FILENAME
Credentials = credentials.Credentials
_credentials_cache = credentials._credentials_cache
configured_access_key = credentials.configured_access_key
get_credentials = credentials.get_credentials
load_credentials = credentials.load_credentials
CHARLIEBOT_HOME_ENV = home.CHARLIEBOT_HOME_ENV
CLAUDE_CONFIG_DIR_ENV_VAR = home.CLAUDE_CONFIG_DIR_ENV_VAR
charliebot_home_dir = home.charliebot_home_dir
default_charliebot_home = home.default_charliebot_home

log = log_once.LazyStructlogLogger()

# Fixed house wall clock pinned by the feature code that stamps chat session names with a timestamp
# and by the feature code that anchors the weekly recycle to Saturday 1 AM. Distinct from the
# scheduled-task default timezone, which a feature owns and which is overridable via
# ``timezone: local`` or any IANA key, so retargeting that default cannot shift these pins.
HOUSE_TIMEZONE = "America/Los_Angeles"

# The profile's config filename, named once: the loader, the reload fingerprint,
# and ``config_file`` must resolve to the same file, and the setup of an isolated
# trial home writes it by that name. A rename that missed one site would leave
# that site silently reading a different file.
CONFIG_FILENAME = "config.yaml"


class HomeService(pydantic.BaseModel):
  """A service this host runs, listed on the /home page and probed for reachability."""

  model_config = pydantic.ConfigDict(extra='forbid')

  name: str  # card title
  description: str  # one line saying what it is for
  url: str  # what the card links to; the probe connects to this URL's host and port


class ServerConfig(pydantic.BaseModel):
  """``server:`` section: the bind address the server listens on."""

  model_config = pydantic.ConfigDict(extra='forbid')

  # The bind address the server listens on. Loopback by default; a host that
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
  # Per-session task cap (processes plus threads), written to the session
  # cgroup's pids.max. A runaway fork loop then fails with EAGAIN inside its
  # own session, before it can exhaust the host's PID space and stop every
  # fork on the host. 0 means no task cap (pids.max = max). With
  # session_memory_max_mb at 0 there is no session cgroup, so no task cap either.
  session_tasks_max: int = 4096

  # Worker-class launch precheck: the filesystems holding the CharlieBot data
  # dir ($CHARLIEBOT_HOME) and the worktree root must hold at least this much
  # free space, or the run stays queued with a blocked report to its parent
  # (an environment install is the write-heavy case). 0 disables the check.
  # Manager turns never check: they write little and are the path that tells
  # the operator about the shortage.
  min_free_disk_gib: int = 10


class PathsConfig(pydantic.BaseModel):
  """``paths:`` section: workspace repos, worker worktrees and scratch directories."""

  model_config = pydantic.ConfigDict(extra='forbid')

  # Workspace directories to scan for git repos
  workspace_dirs: list[str] = ["~/workspace"]

  # Root directory for worker worktrees
  worktree_dir: str = "~/worktrees"

  # Root directory for task scratch directories managed by disk-sweep.
  scratch_dir: str = "~/scripts"

  @pydantic.model_validator(mode="after")
  def _expand_tilde(self) -> PathsConfig:
    """Expand ``~`` in path settings against the process HOME."""
    self.workspace_dirs = [os.path.expanduser(p) for p in self.workspace_dirs]
    self.worktree_dir = os.path.expanduser(self.worktree_dir)
    self.scratch_dir = os.path.expanduser(self.scratch_dir)
    return self


class RoutingConfig(pydantic.BaseModel):
  """``backends.routing:`` section: the host-configured router program that orders unpinned work."""

  model_config = pydantic.ConfigDict(extra='forbid')

  # The router argv (src/features/artifacts/publish.py's publish.command convention): the first
  # item resolves on the server process's PATH. One invocation per selection; stdin and stdout
  # carry the router contract JSON (src/runtime/backend_pool.route). Absent (default): every
  # selection behaves as if the router did not exist.
  command: list[str]


class BackendsConfig(pydantic.BaseModel):
  """``backends:`` section: model-switch options and the rotation pool of unpinned work."""

  model_config = pydantic.ConfigDict(extra='forbid')

  # Backend id rule (the one home of it; other docs point here). An option id is
  # "<type prefix>-<family>" (claude-opus, oc-kimi-k3, clc-glm5-flash), the
  # prefix one of the id prefixes the type's usage source registers (each type's
  # src/backends/<package>/__init__.py). The id names the model family
  # and carries no version: the version lives only in the entry's `model` and
  # `label`. Session, thread and Run metadata, package config files and `pool`
  # store the id, so a version bump edits exactly those two fields of one
  # entry and every stored reference stays valid. The usage tally classifies a retired id
  # (off config, still in old records) by its prefix. An option bound to an
  # account pool may end its id with `-<pool name>`. require_backends refuses a
  # startup whose pool names an id missing from `options`;
  # a package's startup check does the same for the ids in that package's config
  # files, and usage's check_backend_ids refuses an id outside its type's
  # registered prefixes.

  # Pool members in rotation order (src/runtime/backend_pool.py). Unpinned work takes the
  # next member that is not cooling; a member whose quota runs out cools until its reset.
  # Each id must name a backends.options entry. Empty (default): unpinned work runs on the
  # delegating session's backend, the reviewer runs on the worker's backend, and the session
  # namer and page check skip their one-shot.
  pool: list[str] = []

  # The router program that orders unpinned work (None: the pool rotation alone decides, as it
  # always has). The routed roles — implement, quick-edit, script-run, verify and new thread
  # sessions — ask it before the pool; every other caller (cold read, naming, page check, the
  # improve loop) never sees it.
  routing: RoutingConfig | None = None

  # Backend options available for model switching. Each entry parses as the option model that
  # its ``type`` names in the config registry (src/infra/config_registry.py). Additional
  # backends must be configured via $CHARLIEBOT_HOME/config.yaml -> backends.options.
  options: list[Annotated[pydantic.SerializeAsAny[backend_models.BackendOption],
                          pydantic.BeforeValidator(backend_models.parse_option)]] = []


class UiConfig(pydantic.BaseModel):
  """``ui:`` section: the /home page service cards."""

  model_config = pydantic.ConfigDict(extra='forbid')

  # Home page — services this host runs, probed for reachability; default empty. Each card
  # links to the URL and the probe connects to the same host and port.
  home_services: list[HomeService] = []


class TelegramConfig(pydantic.BaseModel):
  """``telegram:`` section: the notification target."""

  model_config = pydantic.ConfigDict(extra='forbid')

  # Telegram notifications
  chat_id: str | None = None


def _alias_field_names(alias: str | pydantic.AliasChoices | pydantic.AliasPath | None) -> set[str]:
  """Flat string names behind an alias declaration, for known-name checks."""
  if isinstance(alias, str):
    return {alias}
  if isinstance(alias, pydantic.AliasChoices):
    return set().union(*(_alias_field_names(choice) for choice in alias.choices))
  if isinstance(alias, pydantic.AliasPath):
    return {str(alias.path[0])}
  return set()


def _locate_in_section(key: str, err: Any) -> Any:
  """One pydantic error detail of section *key*'s own validation, with *key* as the first segment of its location."""
  located = {"type": err["type"], "loc": (key, *err["loc"]), "input": err["input"]}
  if "ctx" in err:
    located["ctx"] = err["ctx"]
  return located


class CharlieBotConfig(pydantic.BaseModel):
  """CharlieBot configuration, loaded from $CHARLIEBOT_HOME/config.yaml.

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

  model_config = pydantic.ConfigDict(extra='allow')

  # Paths — resolved per instantiation so CHARLIEBOT_HOME selects the profile
  charliebot_home: pathlib.Path = pydantic.Field(default_factory=home.charliebot_home_dir)

  # Plan registration page-height gate — absolute path of a headless-chromium-compatible
  # binary on the host running the server. The value stays host-local in config.yaml;
  # nothing in the repo hardcodes a path.
  headless_chrome_bin: str = ""

  server: ServerConfig = pydantic.Field(default_factory=ServerConfig)
  paths: PathsConfig = pydantic.Field(default_factory=PathsConfig)
  backends: BackendsConfig = pydantic.Field(default_factory=BackendsConfig)
  ui: UiConfig = pydantic.Field(default_factory=UiConfig)
  telegram: TelegramConfig = pydantic.Field(default_factory=TelegramConfig)

  @classmethod
  def _field_names(cls) -> set[str]:
    """The top-level names the model declares: its fields and their aliases."""
    names = set(cls.model_fields)
    for field in cls.model_fields.values():
      names |= _alias_field_names(field.alias) | _alias_field_names(field.validation_alias)
    return names

  @pydantic.model_validator(mode='before')
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
      raise pydantic.ValidationError.from_exception_data(
          cls.__name__, [{
              "type": "extra_forbidden",
              "loc": (key,),
              "input": data[key]
          } for key in unknown])
    parsed = dict(data)
    for key, model in sections.items():
      try:
        parsed[key] = model.model_validate(data[key]) if key in data else model()
      except pydantic.ValidationError as exc:
        raise pydantic.ValidationError.from_exception_data(
            cls.__name__, [_locate_in_section(key, err) for err in exc.errors(include_url=False)]) from exc
    return parsed

  @pydantic.model_validator(mode='after')
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
  def sessions_dir(self) -> pathlib.Path:
    return self.charliebot_home / "sessions"

  @property
  def claude_md_file(self) -> pathlib.Path:
    """The master agent prompt: $CHARLIEBOT_HOME/MASTER_AGENT_PROMPT.md."""
    return self.charliebot_home / "MASTER_AGENT_PROMPT.md"

  @property
  def charlie_bot_repo(self) -> pathlib.Path:
    """Root of the charlie-bot repository (derived from package location)."""
    return constants.REPO_ROOT

  @property
  def config_file(self) -> pathlib.Path:
    return self.charliebot_home / CONFIG_FILENAME

  @property
  def credentials_file(self) -> pathlib.Path:
    """The profile's credentials.yaml."""
    return self.charliebot_home / credentials.CREDENTIALS_FILENAME

  @property
  def config_d_dir(self) -> pathlib.Path:
    return self.charliebot_home / "config.d"

  def get_backend_option(self, backend_id: str) -> backend_models.BackendOption | None:
    """Look up a backend option by exact id; None when no entry matches."""
    return next((opt for opt in self.backends.options if opt.id == backend_id), None)

  def discover_repos(self) -> list[dict[str, str]]:
    """Scan paths.workspace_dirs (one level deep) for directories containing a .git folder.

    Returns {"name", "path"} entries with resolved absolute paths, deduplicated
    by path and sorted by name; the endpoint adapters only rename the name key.
    """
    found: dict[str, dict[str, str]] = {}
    for dir_str in self.paths.workspace_dirs:
      parent = pathlib.Path(dir_str)
      if not parent.is_dir():
        continue
      for child in parent.iterdir():
        if not child.is_dir() or not (child / ".git").exists():
          continue
        path = str(child.resolve())
        found.setdefault(path, {"name": child.name, "path": path})
    return sorted(found.values(), key=lambda repo: repo["name"])


def require_backend_option(cfg: CharlieBotConfig, backend_id: str, *, subject: str) -> backend_models.BackendOption:
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


_config_cache = credentials._HotReloadCache(
    fingerprint=_config_fingerprint, event="config_reload_failed", install=_install_config_snapshot)

# Retired config.yaml top-level keys: the loader rejects any file still carrying
# one, and the error names where the key moved. A plain dotted value points into
# the sectioned mapping; a value under :data:`CREDENTIALS_PREFIX` moves into
# credentials.yaml (secrets live there, and the suffix is that file's key path);
# a ``removed...`` value has no successor. A package's own retired keys register
# through :mod:`src.infra.config_registry`; the loader reads both maps.
# Retired keys under ``backends:``, replaced by ``backends.pool``. The loader refuses a file that still
# carries one, so a stale config fails at startup instead of silently dropping its routing.
RETIRED_BACKENDS_KEYS: tuple[str, ...] = ("preference", "worker_default")

LEGACY_KEYS: dict[str, str] = {
    "server_host": "server.host",
    "server_port": "server.port",
    "subprocess_buffer_limit_mb": "server.subprocess_buffer_limit_mb",
    "workspace_dirs": "paths.workspace_dirs",
    "project_dirs": "paths.workspace_dirs",
    "worktree_dir": "paths.worktree_dir",
    "backend_options": "backends.options",
    "model_preference": "backends.pool",
    "home_services": "ui.home_services",
    "telegram_chat_id": "telegram.chat_id",
    config_registry.CREDENTIALS_PREFIX + "telegram_bot_token": "telegram.bot_token",
    config_registry.CREDENTIALS_PREFIX + "charliebot_access_key": "charliebot.access_key",
    config_registry.CREDENTIALS_PREFIX + "moonshot_api_key": "moonshot.api_key",
    config_registry.CREDENTIALS_PREFIX + "aigw_api_key": "aigw.api_key",
    config_registry.CREDENTIALS_PREFIX + "linear_api_key": "linear.api_key",
    config_registry.CREDENTIALS_PREFIX + "feishu_app_id": "feishu.app_id",
    config_registry.CREDENTIALS_PREFIX + "feishu_app_secret": "feishu.app_secret",
    config_registry.CREDENTIALS_PREFIX + "feishu_refresh_token": "feishu.refresh_token",
    config_registry.CREDENTIALS_PREFIX + "feishu_user_access_token": "feishu.user_access_token",
    config_registry.CREDENTIALS_PREFIX + "google_client_id": "google.client_id",
    config_registry.CREDENTIALS_PREFIX + "google_client_secret": "google.client_secret",
    config_registry.CREDENTIALS_PREFIX + "google_refresh_token": "google.refresh_token",
    config_registry.CREDENTIALS_PREFIX + "google_docs_client_id": "google.client_id",
    config_registry.CREDENTIALS_PREFIX + "google_docs_client_secret": "google.client_secret",
    config_registry.CREDENTIALS_PREFIX + "google_docs_refresh_token": "google.refresh_token",
    config_registry.CREDENTIALS_PREFIX + "google_docs_default_folder_id": "google.docs_default_folder_id",
    config_registry.CREDENTIALS_PREFIX + "twitter_api_key": "twitter.api_key",
    config_registry.CREDENTIALS_PREFIX + "twitter_api_secret": "twitter.api_secret",
    config_registry.CREDENTIALS_PREFIX + "twitter_access_token": "twitter.access_token",
    config_registry.CREDENTIALS_PREFIX + "twitter_access_token_secret": "twitter.access_token_secret",
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
  home_dir = home.charliebot_home_dir()
  config_path = home_dir / CONFIG_FILENAME

  config_d = home_dir / "config.d"
  if config_d.is_dir():
    for entry in sorted(config_d.iterdir()):
      if entry.name.endswith(".yaml") and entry.is_file():
        raise ValueError(
            f"{entry} is not a config location: package fragment files live in config.d/ subdirectories; "
            "keys belong in config.yaml (structure) or credentials.yaml (secrets)")

  yaml_data: dict = yaml_utils.load_yaml(config_path, default={})
  legacy_keys = {**LEGACY_KEYS, **config_registry.legacy_keys()}
  legacy_hits = [
      key for key in yaml_data if key in legacy_keys or config_registry.CREDENTIALS_PREFIX + key in legacy_keys
  ]
  if legacy_hits:
    lines = "\n".join(
        f"  {key} -> " + (
            legacy_keys[key] if key in legacy_keys else "credentials.yaml " +
            legacy_keys[config_registry.CREDENTIALS_PREFIX + key]) for key in legacy_hits)
    raise ValueError(f"{config_path} still uses retired top-level keys; move each one:\n{lines}")

  backends_section = yaml_data.get("backends")
  retired_backend_keys = [
      key for key in RETIRED_BACKENDS_KEYS if isinstance(backends_section, dict) and key in backends_section
  ]
  if retired_backend_keys:
    names = ", ".join(f"backends.{key}" for key in retired_backend_keys)
    raise ValueError(
        f"{config_path} still uses retired keys ({names}); list the rotation members under backends.pool "
        "in rotation order instead (each id must name a backends.options entry)")

  # The home directory is chosen by the environment, never by a file that lives
  # inside it: honouring the key would leave the config loaded from one profile and
  # the state written to another, and dropping it silently would hide the mistake.
  if "charliebot_home" in yaml_data:
    raise ValueError(
        f"{config_path} sets 'charliebot_home'; that path is chosen by the "
        f"{home.CHARLIEBOT_HOME_ENV} environment variable. Remove the key.")
  try:
    return CharlieBotConfig(charliebot_home=home_dir, **yaml_data)
  except pydantic.ValidationError as e:
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
  """Raise ValueError when ``backends.options`` or ``backends.pool`` is broken.

  The server calls this once at start, because every session and every package-started
  run resolves a backend id against ``backends.options``; a broken catalog is a
  deployment error worth stopping on. Refused: an empty options list, an option id
  listed twice, and a ``backends.pool`` entry naming no option id. One error lists every
  violation, each line naming the file, the entry and the id.
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
  for index, backend_id in enumerate(cfg.backends.pool):
    if backend_id not in ids:
      problems.append(f"{cfg.config_file}: backends.pool[{index}] names unknown backend '{backend_id}'")
  if problems:
    raise backend_reference_error(problems)


def backend_reference_error(problems: list[str]) -> ValueError:
  """Build the one error every backend-reference check raises, each violation on its own line.

  The catalog check in this module and the per-package startup checks share the
  preamble, so a rejected reference reads the same wherever it is caught.
  """
  return ValueError(
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
