//! `config.yaml` loading: the typed mirror of `src/infra/config.py`.
//!
//! The file is a mapping of sections. The core sections (`server`, `paths`, `backends`, `ui`,
//! `telegram`) and `headless_chrome_bin` parse into the structs below, each refusing unknown keys.
//! A section a package registered ([`registry::CONFIG_SECTIONS`]) stays a raw YAML value until that
//! package's port stage types it, and so do the type-specific fields of a backend option. The checks
//! that run before validation (stray `config.d/*.yaml` files, retired keys, a `charliebot_home` key)
//! and their messages follow `load_config`.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_yaml::{Mapping, Value};

use crate::{paths, pyfmt, registry};

pub const CONFIG_FILENAME: &str = "config.yaml";
pub const CREDENTIALS_FILENAME: &str = "credentials.yaml";

/// Retired keys under `backends:`, replaced by `backends.pool`.
const RETIRED_BACKENDS_KEYS: &[&str] = &["preference", "worker_default"];

/// The top-level names `CharlieBotConfig` declares as fields.
const FIELD_NAMES: &[&str] =
  &["charliebot_home", "headless_chrome_bin", "server", "paths", "backends", "ui", "telegram"];

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
}

fn default_workspace_dirs() -> Vec<String> {
  vec!["~/workspace".to_string()]
}
fn default_worktree_dir() -> String {
  "~/worktrees".to_string()
}

/// One `backends.options[]` entry: the fields every backend type shares, plus its type's own fields.
#[derive(Debug, Clone, PartialEq)]
pub struct BackendOption {
  pub id: String,
  pub label: String,
  pub model: String,
  pub prompt_overlay: Option<String>,
  /// The registered backend type ([`registry::BACKEND_TYPES`]).
  pub backend_type: String,
  /// The type-specific keys, raw until the backend package's port stage types them.
  pub type_fields: Mapping,
}

/// `backends:` section: the rotation pool and the model-switch options.
#[derive(Debug, Clone, PartialEq)]
pub struct BackendsConfig {
  pub pool: Vec<String>,
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
#[derive(Debug, Clone, PartialEq)]
pub struct CharlieBotConfig {
  /// The profile home the file was read from (chosen by `CHARLIEBOT_HOME`, never by the file).
  pub charliebot_home: PathBuf,
  pub headless_chrome_bin: String,
  pub server: ServerConfig,
  pub paths: PathsConfig,
  pub backends: BackendsConfig,
  pub ui: UiConfig,
  pub telegram: TelegramConfig,
  /// The registered package sections the file sets, raw, keyed by section name. A section the file
  /// omits is absent; its package's model defaults apply where the section is read.
  pub sections: BTreeMap<String, Value>,
}

impl CharlieBotConfig {
  pub fn sessions_dir(&self) -> PathBuf {
    self.charliebot_home.join("sessions")
  }

  pub fn config_file(&self) -> PathBuf {
    self.charliebot_home.join(CONFIG_FILENAME)
  }

  pub fn credentials_file(&self) -> PathBuf {
    self.charliebot_home.join(CREDENTIALS_FILENAME)
  }

  pub fn config_d_dir(&self) -> PathBuf {
    self.charliebot_home.join("config.d")
  }

  /// The backend option with exactly this id, or `None`.
  pub fn get_backend_option(&self, backend_id: &str) -> Option<&BackendOption> {
    self.backends.options.iter().find(|option| option.id == backend_id)
  }

  /// The raw value of a registered package section the file sets, or `None` when the file omits it.
  pub fn section(&self, key: &str) -> Option<&Value> {
    self.sections.get(key)
  }
}

/// Load `<home>/config.yaml`.
///
/// Before validation: any `*.yaml` file directly under `config.d/`, any retired top-level key (core or
/// package-registered), a retired `backends:` key, and a `charliebot_home` key each refuse the file. A
/// missing or empty file loads as the schema defaults.
pub fn load_config(home: &Path) -> Result<CharlieBotConfig, ConfigError> {
  let config_path = home.join(CONFIG_FILENAME);
  let config_path_text = paths::display(&config_path);

  let config_d = home.join("config.d");
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
    .filter_map(|key| registry::legacy_destination(key).map(|new| (key.to_string(), new)))
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
      !key.as_str().is_some_and(|name| FIELD_NAMES.contains(&name) || registry::CONFIG_SECTIONS.contains(&name))
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

  let sections = registry::CONFIG_SECTIONS
    .iter()
    .filter_map(|name| data.get(*name).map(|value| (name.to_string(), value.clone())))
    .collect();

  Ok(CharlieBotConfig {
    charliebot_home: home.to_path_buf(),
    headless_chrome_bin,
    server: typed_section(&config_path_text, "server", section("server"), default_server)?,
    paths: paths_config,
    backends: parse_backends(&config_path_text, section("backends"))?,
    ui: typed_section(&config_path_text, "ui", section("ui"), || UiConfig { home_services: Vec::new() })?,
    telegram: typed_section(&config_path_text, "telegram", section("telegram"), || TelegramConfig { chat_id: None })?,
    sections,
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
  PathsConfig { workspace_dirs: default_workspace_dirs(), worktree_dir: default_worktree_dir() }
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

fn parse_backends(config_path: &str, value: Option<Value>) -> Result<BackendsConfig, ConfigError> {
  let mapping = match value {
    None => {
      return Ok(BackendsConfig { pool: Vec::new(), options: Vec::new() });
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
    if !matches!(key.as_str(), Some("pool" | "options")) {
      let shown = key.as_str().map_or_else(|| yaml_key_repr(key), str::to_string);
      return Err(ConfigError(format!("{config_path}: invalid config key backends.{shown}: unknown field")));
    }
  }
  let pool: Vec<String> = typed_section(config_path, "backends.pool", mapping.get("pool").cloned(), Vec::new)?;
  let options = match mapping.get("options") {
    None => Vec::new(),
    Some(Value::Sequence(entries)) => entries
      .iter()
      .enumerate()
      .map(|(index, entry)| parse_option(config_path, index, entry))
      .collect::<Result<_, _>>()?,
    Some(other) => {
      return Err(ConfigError(format!(
        "{config_path}: invalid config key backends.options: expected a list, got {}",
        yaml_value_kind(other)
      )));
    }
  };
  Ok(BackendsConfig { pool, options })
}

/// One backend entry, validated as the option model its `type` names would (`backend_models.parse_option`).
fn parse_option(config_path: &str, index: usize, entry: &Value) -> Result<BackendOption, ConfigError> {
  let location = format!("{config_path}: backends.options[{index}]");
  let Value::Mapping(entry) = entry else {
    return Err(ConfigError(format!("{location}: a backend entry must be a mapping, got {}", yaml_value_kind(entry))));
  };
  let backend_type = match entry.get("type") {
    Some(Value::String(name)) if registry::backend_type(name).is_some() => name.clone(),
    other => {
      let shown = match other {
        Some(Value::String(name)) => pyfmt::str_repr(name),
        Some(value) => yaml_key_repr(value),
        None => "None".to_string(),
      };
      let registered: Vec<&str> = registry::BACKEND_TYPES.iter().map(|backend| backend.name).collect();
      return Err(ConfigError(format!(
        "{location}: backend type {shown} is not registered; registered types: {}",
        registered.join(", ")
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

  const VALID: &str = "server:\n  host: 127.0.0.1\n  port: 18650\nbackends:\n  pool: [claude-fable]\n  options:\n    - id: \
                       claude-fable\n      label: \"CC · Fable\"\n      type: cc-claude\n      model: claude-fable-5-1\n      \
                       effort: xhigh\nvoice:\n  engine: sherpa\n";

  #[test]
  fn a_valid_file_parses_into_typed_sections() {
    let home = home_with(VALID);
    let cfg = load_config(home.path()).unwrap();
    assert_eq!(cfg.server.port, 18650);
    assert_eq!(cfg.server.session_memory_max_mb, 12288);
    assert_eq!(cfg.backends.pool, vec!["claude-fable".to_string()]);
    let option = cfg.get_backend_option("claude-fable").unwrap();
    assert_eq!(
      (option.label.as_str(), option.backend_type.as_str(), option.model.as_str()),
      ("CC · Fable", "cc-claude", "claude-fable-5-1")
    );
    assert_eq!(option.type_fields.get("effort"), Some(&Value::String("xhigh".to_string())));
    assert!(cfg.section("voice").is_some());
    assert!(cfg.section("slack").is_none());
    assert_eq!(cfg.paths.worktree_dir, paths::expanduser("~/worktrees"));
    require_backends(&cfg).unwrap();
  }

  #[test]
  fn an_unknown_top_level_key_is_named() {
    let home = home_with("bogus: 1\nserver: {}\nother_key: x\n");
    let error = load_config(home.path()).unwrap_err().to_string();
    assert!(error.starts_with("unknown config key(s) 'bogus', 'other_key';"), "{error}");
  }

  #[test]
  fn an_unknown_nested_key_and_a_wrong_type_name_the_key() {
    let home = home_with("server:\n  bogus: 1\n");
    let error = load_config(home.path()).unwrap_err().to_string();
    assert!(error.contains("invalid config key server") && error.contains("bogus"), "{error}");
    let home = home_with("server:\n  port: not-a-port\n");
    let error = load_config(home.path()).unwrap_err().to_string();
    assert!(error.contains("invalid config key server.port"), "{error}");
  }

  #[test]
  fn retired_keys_name_their_new_location() {
    let home = home_with("server_port: 1\ncharliebot_access_key: k\n");
    let error = load_config(home.path()).unwrap_err().to_string();
    assert!(
      error.ends_with("still uses retired top-level keys; move each one:\n  server_port -> server.port\n  charliebot_access_key -> credentials.yaml charliebot.access_key"),
      "{error}"
    );
  }

  #[test]
  fn backend_entries_need_a_registered_type_and_a_model() {
    let home = home_with("backends:\n  options:\n    - {id: x, label: X, type: nope, model: m}\n");
    let error = load_config(home.path()).unwrap_err().to_string();
    assert!(error.contains("backend type 'nope' is not registered"), "{error}");
    let home = home_with("backends:\n  options:\n    - {id: x, label: X, type: codex}\n");
    let error = load_config(home.path()).unwrap_err().to_string();
    assert!(error.ends_with("backend 'x' (type 'codex') requires 'model'"), "{error}");
  }

  #[test]
  fn require_backends_lists_every_broken_reference() {
    let home = home_with(
      "backends:\n  pool: [ghost]\n  options:\n    - {id: a, label: A, type: codex, model: m}\n    - {id: a, label: A, type: codex, model: m}\n",
    );
    let cfg = load_config(home.path()).unwrap();
    let error = require_backends(&cfg).unwrap_err().to_string();
    assert!(
      error.contains("backends.options[1] repeats id 'a'")
        && error.contains("backends.pool[0] names unknown backend 'ghost'"),
      "{error}"
    );
    let empty = load_config(home_with("").path()).unwrap();
    assert!(
      require_backends(&empty).unwrap_err().to_string().starts_with("config.yaml: backends.options lists no backend")
    );
  }

  #[test]
  fn a_yaml_file_directly_under_config_d_is_refused() {
    let home = home_with(VALID);
    std::fs::create_dir(home.path().join("config.d")).unwrap();
    std::fs::write(home.path().join("config.d/stray.yaml"), "a: 1").unwrap();
    let error = load_config(home.path()).unwrap_err().to_string();
    assert!(error.contains("stray.yaml is not a config location"), "{error}");
  }
}
