//! `config.yaml` loading: the typed mirror of `src/infra/config.py`.
//!
//! The file is a mapping of sections. The core sections (`server`, `paths`, `backends`, `ui`,
//! `telegram`) and `headless_chrome_bin` parse into the structs below, each refusing unknown keys.
//! A section a package registered ([`ConfigSchema::sections`]) parses with the package's own parser, and
//! the type-specific fields of a backend option stay raw until their backend type's port stage types
//! them. The schema comes from the registered packages (`charliebot-registry` builds it). The checks
//! that run before validation (stray `config.d/*.yaml` files, retired keys, a `charliebot_home` key)
//! and their messages follow `load_config`.

use std::any::Any;
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use serde_yaml::{Mapping, Value};

use crate::{paths, pyfmt};

pub const CONFIG_FILENAME: &str = "config.yaml";
pub const CREDENTIALS_FILENAME: &str = "credentials.yaml";
/// The profile home's directory of session directories, one per session.
pub const SESSIONS_DIR_NAME: &str = "sessions";
/// The profile home's directory of configuration fragments, one YAML file per job or section.
pub const CONFIG_D_DIR_NAME: &str = "config.d";

/// A legacy-key value under this prefix names a `credentials.yaml` key path (`config_registry.CREDENTIALS_PREFIX`).
pub const CREDENTIALS_PREFIX: &str = "credentials: ";

/// Retired keys under `backends:`, replaced by `backends.pool`.
const RETIRED_BACKENDS_KEYS: &[&str] = &["preference", "worker_default"];

/// The top-level names `CharlieBotConfig` declares as fields.
const FIELD_NAMES: &[&str] =
  &["charliebot_home", "headless_chrome_bin", "server", "paths", "backends", "ui", "telegram"];

/// `config.LEGACY_KEYS`: the retired top-level keys the core config names, each with its new location.
const CORE_LEGACY_KEYS: &[(&str, &str)] = &[
  ("server_host", "server.host"),
  ("server_port", "server.port"),
  ("subprocess_buffer_limit_mb", "server.subprocess_buffer_limit_mb"),
  ("workspace_dirs", "paths.workspace_dirs"),
  ("project_dirs", "paths.workspace_dirs"),
  ("worktree_dir", "paths.worktree_dir"),
  ("backend_options", "backends.options"),
  ("model_preference", "backends.pool"),
  ("home_services", "ui.home_services"),
  ("telegram_chat_id", "telegram.chat_id"),
  ("credentials: telegram_bot_token", "telegram.bot_token"),
  ("credentials: charliebot_access_key", "charliebot.access_key"),
  ("credentials: moonshot_api_key", "moonshot.api_key"),
  ("credentials: aigw_api_key", "aigw.api_key"),
  ("credentials: linear_api_key", "linear.api_key"),
  ("credentials: feishu_app_id", "feishu.app_id"),
  ("credentials: feishu_app_secret", "feishu.app_secret"),
  ("credentials: feishu_refresh_token", "feishu.refresh_token"),
  ("credentials: feishu_user_access_token", "feishu.user_access_token"),
  ("credentials: google_client_id", "google.client_id"),
  ("credentials: google_client_secret", "google.client_secret"),
  ("credentials: google_refresh_token", "google.refresh_token"),
  ("credentials: google_docs_client_id", "google.client_id"),
  ("credentials: google_docs_client_secret", "google.client_secret"),
  ("credentials: google_docs_refresh_token", "google.refresh_token"),
  ("credentials: google_docs_default_folder_id", "google.docs_default_folder_id"),
  ("credentials: twitter_api_key", "twitter.api_key"),
  ("credentials: twitter_api_secret", "twitter.api_secret"),
  ("credentials: twitter_access_token", "twitter.access_token"),
  ("credentials: twitter_access_token_secret", "twitter.access_token_secret"),
];

/// A parsed package section: the value its parser returned. [`CharlieBotConfig::section`] downcasts it.
pub type SectionValue = Arc<dyn Any + Send + Sync>;

/// A top-level config key a package owns (`config_registry.register_config_section`). The loader keeps the
/// key's raw YAML and calls `parse` on it; an omitted key arrives as an empty mapping, which yields the
/// model's defaults. A parse error fails the load.
#[derive(Clone, Copy)]
pub struct ConfigSection {
  pub key: &'static str,
  /// The retired top-level keys this section replaced, each with its new location.
  pub legacy_keys: &'static [(&'static str, &'static str)],
  pub parse: fn(&Value) -> Result<SectionValue, String>,
  /// Renders the parsed section the way Python's `model_dump()` dumps it. `charliebot config get <key>`
  /// prints this, so a section's defaults appear the same as the server's own model holds them.
  pub dump: fn(&SectionValue) -> Result<serde_json::Value, String>,
}

/// The value a backend option's Python model holds for a type-specific field the entry omits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OptionFieldDefault {
  /// A required field: the loader has already refused an entry that omits it.
  Required,
  Null,
  Bool(bool),
  /// An empty mapping, like pydantic's `dict` default factory.
  EmptyMap,
}

impl OptionFieldDefault {
  /// The JSON value Python's model dumps for the absent field; `None` for a required field.
  pub fn json(self) -> Option<serde_json::Value> {
    match self {
      OptionFieldDefault::Required => None,
      OptionFieldDefault::Null => Some(serde_json::Value::Null),
      OptionFieldDefault::Bool(flag) => Some(serde_json::Value::Bool(flag)),
      OptionFieldDefault::EmptyMap => Some(serde_json::Value::Object(serde_json::Map::new())),
    }
  }
}

/// How an option field's YAML value reaches JSON.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OptionFieldKind {
  /// The value as YAML loads it.
  AsIs,
  /// A `float` field: pydantic dumps an integer entry as a float (`1` prints `1.0`).
  Float,
}

/// One type-specific field of a `backends.options[]` entry, in its Python model's declaration order. A
/// backend package owns its type's fields; `charliebot config get backends` reads them so an option's dump
/// carries the same fields and defaults as the type's pydantic model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OptionField {
  pub name: &'static str,
  pub default: OptionFieldDefault,
  pub kind: OptionFieldKind,
}

impl OptionField {
  /// A field whose YAML value is dumped as loaded.
  pub const fn field(name: &'static str, default: OptionFieldDefault) -> OptionField {
    OptionField { name, default, kind: OptionFieldKind::AsIs }
  }

  /// A `float` field; `default` is its Python default.
  pub const fn float_field(name: &'static str, default: OptionFieldDefault) -> OptionField {
    OptionField { name, default, kind: OptionFieldKind::Float }
  }
}

/// What the registered packages add to the config schema. `charliebot-registry` builds it at startup.
#[derive(Clone, Default)]
pub struct ConfigSchema {
  pub sections: Vec<ConfigSection>,
  /// Retired top-level keys that no section owns (`config_registry.register_legacy_keys`).
  pub legacy_keys: Vec<(&'static str, &'static str)>,
  /// The registered backend type names (`backend_type_registration.register_backend_type`).
  pub backend_types: Vec<&'static str>,
}

/// The parsed package sections of one loaded config, by key.
#[derive(Clone, Default)]
pub struct Sections(BTreeMap<&'static str, SectionValue>);

impl fmt::Debug for Sections {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_list().entries(self.0.keys()).finish()
  }
}

/// A config file the loader refuses; the message names the file and the offending key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(pub String);

impl fmt::Display for ConfigError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(&self.0)
  }
}

impl std::error::Error for ConfigError {}

/// `server:` section: the bind address and the per-session process limits.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
  #[serde(default = "default_host")]
  pub host: String,
  #[serde(default = "default_port")]
  pub port: u16,
  #[serde(default = "default_subprocess_buffer_limit_mb")]
  pub subprocess_buffer_limit_mb: i64,
  #[serde(default = "default_session_memory_max_mb")]
  pub session_memory_max_mb: i64,
  #[serde(default)]
  pub session_swap_max_mb: i64,
  #[serde(default = "default_session_tasks_max")]
  pub session_tasks_max: i64,
  #[serde(default = "default_min_free_disk_gib")]
  pub min_free_disk_gib: i64,
}

// The schema defaults of `ServerConfig` in src/infra/config.py.
fn default_host() -> String {
  "127.0.0.1".to_string()
}
fn default_port() -> u16 {
  18498
}
fn default_subprocess_buffer_limit_mb() -> i64 {
  1024
}
fn default_session_memory_max_mb() -> i64 {
  12288
}
fn default_session_tasks_max() -> i64 {
  4096
}
fn default_min_free_disk_gib() -> i64 {
  10
}

/// `paths:` section: the repos to scan and where worker worktrees live, `~` expanded.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathsConfig {
  #[serde(default = "default_workspace_dirs")]
  pub workspace_dirs: Vec<String>,
  #[serde(default = "default_worktree_dir")]
  pub worktree_dir: String,
  #[serde(default = "default_scratch_dir")]
  pub scratch_dir: String,
}

fn default_workspace_dirs() -> Vec<String> {
  vec!["~/workspace".to_string()]
}
fn default_worktree_dir() -> String {
  "~/worktrees".to_string()
}
fn default_scratch_dir() -> String {
  "~/scripts".to_string()
}

/// One `backends.options[]` entry: the fields every backend type shares, plus its type's own fields.
#[derive(Debug, Clone, PartialEq)]
pub struct BackendOption {
  pub id: String,
  pub label: String,
  pub model: String,
  pub prompt_overlay: Option<String>,
  /// The registered backend type, one of [`ConfigSchema::backend_types`].
  pub backend_type: String,
  /// The type-specific keys, raw until the backend package's port stage types them.
  pub type_fields: Mapping,
}

/// `backends.routing:` section: the host command that orders unpinned backend candidates.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingConfig {
  pub command: Vec<String>,
}

/// `backends:` section: the rotation pool, optional router command, and model-switch options.
#[derive(Debug, Clone, PartialEq)]
pub struct BackendsConfig {
  pub pool: Vec<String>,
  pub routing: Option<RoutingConfig>,
  pub options: Vec<BackendOption>,
}

/// One `/home` page service card.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HomeService {
  pub name: String,
  pub description: String,
  pub url: String,
}

/// `ui:` section: the `/home` page service cards.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiConfig {
  #[serde(default)]
  pub home_services: Vec<HomeService>,
}

/// `telegram:` section: the notification target.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelegramConfig {
  #[serde(default)]
  pub chat_id: Option<String>,
}

/// One profile's parsed `config.yaml`.
#[derive(Debug, Clone)]
pub struct CharlieBotConfig {
  /// The profile home the file was read from (chosen by `CHARLIEBOT_HOME`, never by the file).
  pub charliebot_home: PathBuf,
  pub headless_chrome_bin: String,
  pub server: ServerConfig,
  pub paths: PathsConfig,
  pub backends: BackendsConfig,
  pub ui: UiConfig,
  pub telegram: TelegramConfig,
  /// Every registered package section, parsed by its package's parser. An omitted section holds the
  /// parser's result for an empty mapping, which is the package's defaults.
  pub sections: Sections,
}

/// The sessions directory of a profile home.
pub fn sessions_dir_in(home: &Path) -> PathBuf {
  home.join(SESSIONS_DIR_NAME)
}

impl CharlieBotConfig {
  pub fn sessions_dir(&self) -> PathBuf {
    sessions_dir_in(&self.charliebot_home)
  }

  pub fn config_file(&self) -> PathBuf {
    self.charliebot_home.join(CONFIG_FILENAME)
  }

  pub fn credentials_file(&self) -> PathBuf {
    self.charliebot_home.join(CREDENTIALS_FILENAME)
  }

  pub fn config_d_dir(&self) -> PathBuf {
    self.charliebot_home.join(CONFIG_D_DIR_NAME)
  }

  /// The backend option with exactly this id, or `None`.
  pub fn get_backend_option(&self, backend_id: &str) -> Option<&BackendOption> {
    self.backends.options.iter().find(|option| option.id == backend_id)
  }

  /// The parsed value of the registered section `key`, as its package's parser returned it.
  ///
  /// Panics when no package registered `key` or `T` is not the parser's type: both are programming
  /// errors, so the failure names the key.
  pub fn section<T: Any + Send + Sync>(&self, key: &str) -> &T {
    let value = self.sections.0.get(key).unwrap_or_else(|| panic!("config section {key:?} is not registered"));
    value.downcast_ref::<T>().unwrap_or_else(|| panic!("config section {key:?} does not hold the requested type"))
  }

  /// The parsed value of the registered section `key`, or `None` when no package registered it. A reader that
  /// only renders the value (`charliebot config get`) uses this instead of [`Self::section`].
  pub fn section_value(&self, key: &str) -> Option<&SectionValue> {
    self.sections.0.get(key)
  }
}

/// The retired top-level key `key` moved to, as the Python loader prints it, or `None` for a live key.
///
/// A structure key maps to its dotted location; a secret maps to `credentials.yaml <key path>`. The schema's
/// legacy keys include every package section's keys.
fn legacy_destination(key: &str, schema: &ConfigSchema) -> Option<String> {
  let legacy: Vec<(&str, &str)> = CORE_LEGACY_KEYS.iter().copied().chain(schema.legacy_keys.iter().copied()).collect();
  let lookup = |name: &str| legacy.iter().find(|(old, _)| *old == name).map(|(_, new)| new.to_string());
  if let Some(new) = lookup(key) {
    return Some(new);
  }
  lookup(&format!("{CREDENTIALS_PREFIX}{key}")).map(|new| format!("credentials.yaml {new}"))
}

/// Load `<home>/config.yaml` against the registered packages' `schema`.
///
/// Before validation: any `*.yaml` file directly under `config.d/`, any retired top-level key (core or
/// package-registered), a retired `backends:` key, and a `charliebot_home` key each refuse the file. A
/// missing or empty file loads as the schema defaults. Each registered section is parsed; a parse error
/// fails the load.
pub fn load_config(home: &Path, schema: &ConfigSchema) -> Result<CharlieBotConfig, ConfigError> {
  let config_path = home.join(CONFIG_FILENAME);
  let config_path_text = paths::display(&config_path);

  let config_d = home.join(CONFIG_D_DIR_NAME);
  if config_d.is_dir() {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&config_d)
      .map_err(|error| ConfigError(format!("{}: {error}", paths::display(&config_d))))?
      .map(|entry| entry.map(|entry| entry.path()))
      .collect::<Result<_, _>>()
      .map_err(|error| ConfigError(format!("{}: {error}", paths::display(&config_d))))?;
    entries.sort();
    for entry in entries {
      if entry.file_name().is_some_and(|name| name.to_string_lossy().ends_with(".yaml")) && entry.is_file() {
        return Err(ConfigError(format!(
          "{} is not a config location: package fragment files live in config.d/ subdirectories; keys belong in \
           config.yaml (structure) or credentials.yaml (secrets)",
          paths::display(&entry)
        )));
      }
    }
  }

  let data = read_yaml_mapping(&config_path)?;

  let legacy_hits: Vec<(String, String)> = data
    .keys()
    .filter_map(Value::as_str)
    .filter_map(|key| legacy_destination(key, schema).map(|new| (key.to_string(), new)))
    .collect();
  if !legacy_hits.is_empty() {
    let lines: Vec<String> = legacy_hits.iter().map(|(key, new)| format!("  {key} -> {new}")).collect();
    return Err(ConfigError(format!(
      "{config_path_text} still uses retired top-level keys; move each one:\n{}",
      lines.join("\n")
    )));
  }

  if let Some(Value::Mapping(backends)) = data.get("backends") {
    let retired: Vec<String> = RETIRED_BACKENDS_KEYS
      .iter()
      .filter(|key| backends.contains_key(**key))
      .map(|key| format!("backends.{key}"))
      .collect();
    if !retired.is_empty() {
      return Err(ConfigError(format!(
        "{config_path_text} still uses retired keys ({}); list the rotation members under backends.pool in rotation \
         order instead (each id must name a backends.options entry)",
        retired.join(", ")
      )));
    }
  }

  if data.contains_key("charliebot_home") {
    return Err(ConfigError(format!(
      "{config_path_text} sets 'charliebot_home'; that path is chosen by the {} environment variable. Remove the key.",
      crate::home::CHARLIEBOT_HOME_ENV
    )));
  }

  let unknown: Vec<String> = data
    .keys()
    .filter(|key| {
      !key
        .as_str()
        .is_some_and(|name| FIELD_NAMES.contains(&name) || schema.sections.iter().any(|section| section.key == name))
    })
    .map(yaml_key_repr)
    .collect();
  if !unknown.is_empty() {
    return Err(ConfigError(format!(
      "unknown config key(s) {}; declare the key(s) on CharlieBotConfig, register a config section for them, or \
       remove them",
      unknown.join(", ")
    )));
  }

  let section = |name: &str| data.get(name).cloned();
  let mut paths_config: PathsConfig = typed_section(&config_path_text, "paths", section("paths"), default_paths)?;
  paths_config.workspace_dirs = paths_config.workspace_dirs.iter().map(|dir| paths::expanduser(dir)).collect();
  paths_config.worktree_dir = paths::expanduser(&paths_config.worktree_dir);
  paths_config.scratch_dir = paths::expanduser(&paths_config.scratch_dir);

  let headless_chrome_bin = match data.get("headless_chrome_bin") {
    None => String::new(),
    Some(Value::String(text)) => text.clone(),
    Some(other) => {
      return Err(ConfigError(format!(
        "{config_path_text}: headless_chrome_bin: expected a string, got {}",
        yaml_value_kind(other)
      )));
    }
  };

  let mut sections = BTreeMap::new();
  for package_section in &schema.sections {
    let raw = data.get(package_section.key).cloned().unwrap_or_else(|| Value::Mapping(Mapping::new()));
    let parsed = (package_section.parse)(&raw).map_err(|message| {
      ConfigError(format!("{config_path_text}: invalid config key {}: {message}", package_section.key))
    })?;
    sections.insert(package_section.key, parsed);
  }

  Ok(CharlieBotConfig {
    charliebot_home: home.to_path_buf(),
    headless_chrome_bin,
    server: typed_section(&config_path_text, "server", section("server"), default_server)?,
    paths: paths_config,
    backends: parse_backends(&config_path_text, section("backends"), &schema.backend_types)?,
    ui: typed_section(&config_path_text, "ui", section("ui"), || UiConfig { home_services: Vec::new() })?,
    telegram: typed_section(&config_path_text, "telegram", section("telegram"), || TelegramConfig { chat_id: None })?,
    sections: Sections(sections),
  })
}

/// Refuse a config whose backend catalog is broken: no options, an id listed twice, or a pool entry
/// naming no option. One error lists every violation (`config.require_backends`).
pub fn require_backends(cfg: &CharlieBotConfig) -> Result<(), ConfigError> {
  if cfg.backends.options.is_empty() {
    return Err(ConfigError(
      "config.yaml: backends.options lists no backend; copy the starter entries from configs/config.example.yaml"
        .to_string(),
    ));
  }
  let config_file = paths::display(&cfg.config_file());
  let mut problems = Vec::new();
  let mut ids = std::collections::HashSet::new();
  for (index, option) in cfg.backends.options.iter().enumerate() {
    if !ids.insert(option.id.as_str()) {
      problems.push(format!("{config_file}: backends.options[{index}] repeats id '{}'", option.id));
    }
  }
  for (index, backend_id) in cfg.backends.pool.iter().enumerate() {
    if !ids.contains(backend_id.as_str()) {
      problems.push(format!("{config_file}: backends.pool[{index}] names unknown backend '{backend_id}'"));
    }
  }
  if problems.is_empty() {
    return Ok(());
  }
  Err(ConfigError(format!(
    "backend references must name a backends.options id (id rule: BackendsConfig in src/infra/config.py):\n{}",
    problems.iter().map(|problem| format!("  {problem}")).collect::<Vec<_>>().join("\n")
  )))
}

/// The YAML document at `path`; a missing or empty file reads as `Null`.
pub fn read_yaml_document(path: &Path) -> Result<Value, ConfigError> {
  let path_text = paths::display(path);
  let text = match std::fs::read_to_string(path) {
    Ok(text) => text,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Value::Null),
    Err(error) => return Err(ConfigError(format!("{path_text}: {error}"))),
  };
  serde_yaml::from_str(&text).map_err(|error| ConfigError(format!("{path_text}: {error}")))
}

fn read_yaml_mapping(path: &Path) -> Result<Mapping, ConfigError> {
  match read_yaml_document(path)? {
    Value::Null => Ok(Mapping::new()),
    Value::Mapping(mapping) => Ok(mapping),
    other => Err(ConfigError(format!(
      "{}: the document must be a mapping of sections, got {}",
      paths::display(path),
      yaml_value_kind(&other)
    ))),
  }
}

fn default_server() -> ServerConfig {
  ServerConfig {
    host: default_host(),
    port: default_port(),
    subprocess_buffer_limit_mb: default_subprocess_buffer_limit_mb(),
    session_memory_max_mb: default_session_memory_max_mb(),
    session_swap_max_mb: 0,
    session_tasks_max: default_session_tasks_max(),
    min_free_disk_gib: default_min_free_disk_gib(),
  }
}

fn default_paths() -> PathsConfig {
  PathsConfig {
    workspace_dirs: default_workspace_dirs(),
    worktree_dir: default_worktree_dir(),
    scratch_dir: default_scratch_dir(),
  }
}

/// Deserialize one core section; an absent section takes `absent()`, the model's defaults.
fn typed_section<T: serde::de::DeserializeOwned>(
  config_path: &str,
  name: &str,
  value: Option<Value>,
  absent: impl FnOnce() -> T,
) -> Result<T, ConfigError> {
  match value {
    None => Ok(absent()),
    Some(value) => serde_path_to_error::deserialize(value).map_err(|error| {
      let location = error.path().to_string();
      let key = match location.as_str() {
        "." => name.to_string(),
        indexed if indexed.starts_with('[') => format!("{name}{indexed}"),
        nested => format!("{name}.{nested}"),
      };
      ConfigError(format!("{config_path}: invalid config key {key}: {}", error.into_inner()))
    }),
  }
}

fn parse_backends(
  config_path: &str,
  value: Option<Value>,
  backend_types: &[&str],
) -> Result<BackendsConfig, ConfigError> {
  let mapping = match value {
    None => {
      return Ok(BackendsConfig { pool: Vec::new(), routing: None, options: Vec::new() });
    }
    Some(Value::Mapping(mapping)) => mapping,
    Some(other) => {
      return Err(ConfigError(format!(
        "{config_path}: invalid config key backends: expected a mapping, got {}",
        yaml_value_kind(&other)
      )));
    }
  };
  for key in mapping.keys() {
    if !matches!(key.as_str(), Some("pool" | "options" | "routing")) {
      let shown = key.as_str().map_or_else(|| yaml_key_repr(key), str::to_string);
      return Err(ConfigError(format!("{config_path}: invalid config key backends.{shown}: unknown field")));
    }
  }
  let routing: Option<RoutingConfig> =
    typed_section(config_path, "backends.routing", mapping.get("routing").cloned(), || None)?;
  let pool: Vec<String> = typed_section(config_path, "backends.pool", mapping.get("pool").cloned(), Vec::new)?;
  let options = match mapping.get("options") {
    None => Vec::new(),
    Some(Value::Sequence(entries)) => entries
      .iter()
      .enumerate()
      .map(|(index, entry)| parse_option(config_path, index, entry, backend_types))
      .collect::<Result<_, _>>()?,
    Some(other) => {
      return Err(ConfigError(format!(
        "{config_path}: invalid config key backends.options: expected a list, got {}",
        yaml_value_kind(other)
      )));
    }
  };
  Ok(BackendsConfig { pool, routing, options })
}

/// One backend entry, validated as the option model its `type` names would (`backend_models.parse_option`).
fn parse_option(
  config_path: &str,
  index: usize,
  entry: &Value,
  backend_types: &[&str],
) -> Result<BackendOption, ConfigError> {
  let location = format!("{config_path}: backends.options[{index}]");
  let Value::Mapping(entry) = entry else {
    return Err(ConfigError(format!("{location}: a backend entry must be a mapping, got {}", yaml_value_kind(entry))));
  };
  let backend_type = match entry.get("type") {
    Some(Value::String(name)) if backend_types.contains(&name.as_str()) => name.clone(),
    other => {
      let shown = match other {
        Some(Value::String(name)) => pyfmt::str_repr(name),
        Some(value) => yaml_key_repr(value),
        None => "None".to_string(),
      };
      return Err(ConfigError(format!(
        "{location}: backend type {shown} is not registered; registered types: {}",
        backend_types.join(", ")
      )));
    }
  };
  let required_text = |key: &str| -> Result<String, ConfigError> {
    match entry.get(key) {
      Some(Value::String(text)) => Ok(text.clone()),
      Some(other) => Err(ConfigError(format!("{location}.{key}: expected a string, got {}", yaml_value_kind(other)))),
      None => Err(ConfigError(format!("{location}.{key}: field required"))),
    }
  };
  let optional_text = |key: &str| -> Result<Option<String>, ConfigError> {
    match entry.get(key) {
      None | Some(Value::Null) => Ok(None),
      Some(Value::String(text)) => Ok(Some(text.clone())),
      Some(other) => Err(ConfigError(format!("{location}.{key}: expected a string, got {}", yaml_value_kind(other)))),
    }
  };
  let id = required_text("id")?;
  let label = required_text("label")?;
  let model = optional_text("model")?
    .ok_or_else(|| ConfigError(format!("{location}: backend '{id}' (type '{backend_type}') requires 'model'")))?;
  let prompt_overlay = optional_text("prompt_overlay")?;
  let type_fields: Mapping = entry
    .iter()
    .filter(|(key, _)| !matches!(key.as_str(), Some("id" | "label" | "model" | "prompt_overlay" | "type")))
    .map(|(key, value)| (key.clone(), value.clone()))
    .collect();
  Ok(BackendOption { id, label, model, prompt_overlay, backend_type, type_fields })
}

/// A YAML mapping key as CPython's `repr` prints the value PyYAML loaded for it.
pub fn yaml_key_repr(key: &Value) -> String {
  match key {
    Value::String(text) => pyfmt::str_repr(text),
    Value::Null => "None".to_string(),
    Value::Bool(true) => "True".to_string(),
    Value::Bool(false) => "False".to_string(),
    Value::Number(number) => number.to_string(),
    other => format!("{other:?}"),
  }
}

/// Convert a YAML mapping from an option's type fields to a JSON object. A JSON object key must be a string, and
/// YAML tags and non-finite floats have no JSON form.
pub fn yaml_mapping_to_json(mapping: &Mapping) -> Result<serde_json::Map<String, serde_json::Value>, String> {
  let mut object = serde_json::Map::new();
  for (key, value) in mapping {
    let Some(key) = key.as_str() else {
      return Err(format!("a JSON object key must be a string, not {}", yaml_key_repr(key)));
    };
    object.insert(key.to_string(), yaml_to_json(value)?);
  }
  Ok(object)
}

/// Convert one YAML value to its JSON form, under the same key and tag rules as [`yaml_mapping_to_json`].
pub fn yaml_to_json(value: &Value) -> Result<serde_json::Value, String> {
  match value {
    Value::Null => Ok(serde_json::Value::Null),
    Value::Bool(flag) => Ok(serde_json::Value::Bool(*flag)),
    Value::Number(number) => serde_json::to_value(number).map_err(|error| error.to_string()),
    Value::String(text) => Ok(serde_json::Value::String(text.clone())),
    Value::Sequence(items) => {
      items.iter().map(yaml_to_json).collect::<Result<Vec<_>, _>>().map(serde_json::Value::Array)
    }
    Value::Mapping(mapping) => yaml_mapping_to_json(mapping).map(serde_json::Value::Object),
    Value::Tagged(_) => Err("a tagged YAML value has no JSON form".to_string()),
  }
}

/// The kind of a YAML value, for messages that reject it.
pub fn yaml_value_kind(value: &Value) -> &'static str {
  match value {
    Value::Null => "null",
    Value::Bool(_) => "a boolean",
    Value::Number(_) => "a number",
    Value::String(_) => "a string",
    Value::Sequence(_) => "a list",
    Value::Mapping(_) => "a mapping",
    Value::Tagged(_) => "a tagged value",
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn home_with(config: &str) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join(CONFIG_FILENAME), config).unwrap();
    home
  }

  /// The schema the tests load against: a `voice` section that keeps its raw YAML, and two backend types.
  fn test_schema() -> ConfigSchema {
    fn raw(value: &Value) -> Result<SectionValue, String> {
      Ok(Arc::new(value.clone()))
    }
    fn dump(value: &SectionValue) -> Result<serde_json::Value, String> {
      let value = value.downcast_ref::<Value>().ok_or("the voice section holds a raw YAML value")?;
      yaml_to_json(value)
    }
    ConfigSchema {
      sections: vec![ConfigSection { key: "voice", legacy_keys: &[], parse: raw, dump }],
      legacy_keys: Vec::new(),
      backend_types: vec!["cc-claude", "codex"],
    }
  }

  fn load(home: &tempfile::TempDir) -> Result<CharlieBotConfig, ConfigError> {
    load_config(home.path(), &test_schema())
  }

  const VALID: &str = "server:\n  host: 127.0.0.1\n  port: 18650\nbackends:\n  pool: [claude-fable]\n  options:\n    - id: \
                       claude-fable\n      label: \"CC · Fable\"\n      type: cc-claude\n      model: claude-fable-5-1\n      \
                       effort: xhigh\nvoice: {}\n";

  #[test]
  fn a_valid_file_parses_into_typed_sections() {
    let home = home_with(VALID);
    let cfg = load(&home).unwrap();
    assert_eq!(cfg.server.port, 18650);
    assert_eq!(cfg.server.session_memory_max_mb, 12288);
    assert_eq!(cfg.backends.pool, vec!["claude-fable".to_string()]);
    let option = cfg.get_backend_option("claude-fable").unwrap();
    assert_eq!(
      (option.label.as_str(), option.backend_type.as_str(), option.model.as_str()),
      ("CC · Fable", "cc-claude", "claude-fable-5-1")
    );
    assert_eq!(option.type_fields.get("effort"), Some(&Value::String("xhigh".to_string())));
    assert!(cfg.section::<Value>("voice").is_mapping());
    assert_eq!(cfg.paths.worktree_dir, paths::expanduser("~/worktrees"));
    assert_eq!(cfg.paths.scratch_dir, paths::expanduser("~/scripts"));
    require_backends(&cfg).unwrap();
  }

  #[test]
  fn scratch_dir_uses_its_default_and_expands_a_configured_tilde() {
    let home = home_with(VALID);
    let cfg = load(&home).unwrap();
    assert_eq!(cfg.paths.scratch_dir, paths::expanduser("~/scripts"));

    let config_text = format!("{VALID}paths:\n  scratch_dir: ~/synthetic-scripts\n");
    let home = home_with(&config_text);
    let cfg = load(&home).unwrap();
    assert_eq!(cfg.paths.scratch_dir, paths::expanduser("~/synthetic-scripts"));
  }

  #[test]
  fn an_unknown_top_level_key_is_named() {
    let home = home_with("bogus: 1\nserver: {}\nother_key: x\n");
    let error = load(&home).unwrap_err().to_string();
    assert!(error.starts_with("unknown config key(s) 'bogus', 'other_key';"), "{error}");
  }

  #[test]
  fn removed_voice_legacy_keys_are_unknown_top_level_keys() {
    for key in ["voice_engine", "voice_model_id"] {
      let home = home_with(&format!("{key}: synthetic\n"));
      let error = load(&home).unwrap_err().to_string();
      assert!(error.starts_with("unknown config key(s)"), "{error}");
      assert!(error.contains(key), "{error}");
    }
  }

  #[test]
  fn an_unknown_nested_key_and_a_wrong_type_name_the_key() {
    let home = home_with("server:\n  bogus: 1\n");
    let error = load(&home).unwrap_err().to_string();
    assert!(error.contains("invalid config key server") && error.contains("bogus"), "{error}");
    let home = home_with("server:\n  port: not-a-port\n");
    let error = load(&home).unwrap_err().to_string();
    assert!(error.contains("invalid config key server.port"), "{error}");
  }

  #[test]
  fn retired_keys_name_their_new_location() {
    let home = home_with("server_port: 1\ncharliebot_access_key: k\n");
    let error = load(&home).unwrap_err().to_string();
    assert!(
      error.ends_with("still uses retired top-level keys; move each one:\n  server_port -> server.port\n  charliebot_access_key -> credentials.yaml charliebot.access_key"),
      "{error}"
    );
  }

  #[test]
  fn backend_entries_need_a_registered_type_and_a_model() {
    let home = home_with("backends:\n  options:\n    - {id: x, label: X, type: nope, model: m}\n");
    let error = load(&home).unwrap_err().to_string();
    assert!(error.contains("backend type 'nope' is not registered"), "{error}");
    let home = home_with("backends:\n  options:\n    - {id: x, label: X, type: codex}\n");
    let error = load(&home).unwrap_err().to_string();
    assert!(error.ends_with("backend 'x' (type 'codex') requires 'model'"), "{error}");
  }

  #[test]
  fn routing_command_is_optional_and_rejects_extra_fields() {
    let home = home_with("backends:\n  routing:\n    command: [synthetic-router, --rank]\n");
    let cfg = load(&home).unwrap();
    assert_eq!(cfg.backends.routing.as_ref().unwrap().command, ["synthetic-router", "--rank"]);

    let absent = load(&home_with("backends: {}\n")).unwrap();
    assert!(absent.backends.routing.is_none());

    let home = home_with("backends:\n  routing:\n    command: [synthetic-router]\n    extra: refused\n");
    let error = load(&home).unwrap_err().to_string();
    assert!(error.contains("backends.routing.extra") && error.contains("unknown field"), "{error}");
  }

  #[test]
  fn require_backends_lists_every_broken_reference() {
    let home = home_with(
      "backends:\n  pool: [ghost]\n  options:\n    - {id: a, label: A, type: codex, model: m}\n    - {id: a, label: A, type: codex, model: m}\n",
    );
    let cfg = load(&home).unwrap();
    let error = require_backends(&cfg).unwrap_err().to_string();
    assert!(
      error.contains("backends.options[1] repeats id 'a'")
        && error.contains("backends.pool[0] names unknown backend 'ghost'"),
      "{error}"
    );
    let empty = load(&home_with("")).unwrap();
    assert!(
      require_backends(&empty).unwrap_err().to_string().starts_with("config.yaml: backends.options lists no backend")
    );
  }

  #[test]
  fn a_yaml_file_directly_under_config_d_is_refused() {
    let home = home_with(VALID);
    std::fs::create_dir(home.path().join("config.d")).unwrap();
    std::fs::write(home.path().join("config.d/stray.yaml"), "a: 1").unwrap();
    let error = load(&home).unwrap_err().to_string();
    assert!(error.contains("stray.yaml is not a config location"), "{error}");
  }

  #[test]
  fn a_config_yaml_that_sets_the_home_is_refused() {
    let home = home_with("charliebot_home: /tmp/elsewhere\n");
    let error = load(&home).unwrap_err().to_string();
    assert!(error.contains("sets 'charliebot_home'") && error.contains("CHARLIEBOT_HOME"), "{error}");
  }

  #[test]
  fn a_config_loads_from_the_selected_home() {
    let home = home_with("server:\n  port: 19999\n");
    let selected = crate::home::resolve_home(home.path().to_str().unwrap()).unwrap();
    let cfg = load_config(&selected, &test_schema()).unwrap();
    assert_eq!(cfg.charliebot_home, selected);
    assert_eq!(cfg.server.port, 19999);
    assert_eq!(cfg.sessions_dir(), selected.join("sessions"));
  }
}
