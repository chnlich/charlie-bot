//! `config.yaml` loading: the typed configuration model.
//!
//! The file is a mapping of sections. The core sections (`server`, `paths`, `backends`, `ui`,
//! `telegram`) and `headless_chrome_bin` parse into the structs below, each refusing unknown keys.
//! A section a package registered ([`ConfigSchema::sections`]) parses with the package's own parser, and
//! the type-specific fields of a backend option stay raw until their backend type's package parses
//! them. The schema comes from the registered packages (`charliebot-registry` builds it). The checks
//! that run before validation (stray `config.d/*.yaml` files, retired keys, a `charliebot_home` key)
//! and their messages follow `load_config`.

use std::any::Any;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use serde_yaml::{Mapping, Value};

use crate::{paths, text_repr};

pub const CONFIG_FILENAME: &str = "config.yaml";
pub const CREDENTIALS_FILENAME: &str = "credentials.yaml";
/// The profile home's directory of session directories, one per session.
pub const SESSIONS_DIR_NAME: &str = "sessions";
/// The profile home's directory of configuration fragments, one YAML file per job or section.
pub const CONFIG_D_DIR_NAME: &str = "config.d";
/// The `config.d` subdirectory that holds the scheduled job files, one YAML file per job.
pub const CRON_D_DIR_NAME: &str = "cron.d";

/// A legacy-key value under this prefix names a `credentials.yaml` key path (`CREDENTIALS_PREFIX`).
pub const CREDENTIALS_PREFIX: &str = "credentials: ";

/// The `api_base` key of a `backends.options[]` entry: the upstream endpoint several backend types read. They import
/// this name instead of each spelling it.
pub const API_BASE_KEY: &str = "api_base";

/// The `credential` key of a `backends.options[]` entry: the `credentials.yaml` section whose `api_key` authenticates
/// the entry. The option schemas and the CLI's readers import this name instead of each spelling it.
pub const CREDENTIAL_KEY: &str = "credential";

/// The `context_window` key of a `backends.options[]` entry: the declared window in tokens. The option schemas and
/// the CLI's reviewer import this name instead of each spelling it.
pub const CONTEXT_WINDOW_KEY: &str = "context_window";

/// The `proxy_url` key of a `backends.options[]` entry: the proxy the backend's child process routes its outbound
/// traffic through. The option schemas import this name instead of each spelling it.
pub const PROXY_URL_KEY: &str = "proxy_url";

/// The `extra_body` key of a `backends.options[]` entry: the request-body fields the backend adds to the top level of
/// every request it sends. The option schemas import this name instead of each spelling it.
pub const EXTRA_BODY_KEY: &str = "extra_body";

/// Retired keys under `backends:`, replaced by `backends.pools` and `backends.calls`.
const RETIRED_BACKENDS_KEYS: &[&str] = &["preference", "worker_default", "auto", "pool"];

/// The top-level names `CharlieBotConfig` declares as fields.
const FIELD_NAMES: &[&str] =
  &["charliebot_home", "headless_chrome_bin", "server", "paths", "backends", "ui", "telegram", "remote_run", "github"];

/// `CORE_LEGACY_KEYS`: the retired top-level keys the core config names, each with its new location.
const CORE_LEGACY_KEYS: &[(&str, &str)] = &[
  ("server_host", "server.host"),
  ("server_port", "server.port"),
  ("subprocess_buffer_limit_mb", "server.subprocess_buffer_limit_mb"),
  ("workspace_dirs", "paths.workspace_dirs"),
  ("project_dirs", "paths.workspace_dirs"),
  ("worktree_dir", "paths.worktree_dir"),
  ("backend_options", "backends.options"),
  ("model_preference", "backends.pools"),
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

/// A top-level config key a package owns. The loader keeps the
/// key's raw YAML and calls `parse` on it; an omitted key arrives as an empty mapping, which yields the
/// model's defaults. A parse error fails the load.
#[derive(Clone, Copy)]
pub struct ConfigSection {
  pub key: &'static str,
  /// The retired top-level keys this section replaced, each with its new location.
  pub legacy_keys: &'static [(&'static str, &'static str)],
  pub parse: fn(&Value) -> Result<SectionValue, String>,
  /// Renders the parsed section as the configuration model dumps it. `charliebot config get <key>`
  /// prints this, so a section's defaults appear the same as the server's own model holds them.
  pub dump: fn(&SectionValue) -> Result<serde_json::Value, String>,
}

/// The value a backend option's model holds for a type-specific field the entry omits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OptionFieldDefault {
  /// A required field: the loader has already refused an entry that omits it.
  Required,
  Null,
  Bool(bool),
  /// An empty mapping, the shape a default factory produces.
  EmptyMap,
}

impl OptionFieldDefault {
  /// The JSON value the model dumps for the absent field; `None` for a required field.
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
  /// A `float` field: an integer entry dumps as a float (`1` prints `1.0`).
  Float,
}

/// One type-specific field of a `backends.options[]` entry, in its model's declaration order. A
/// backend package owns its type's fields; `charliebot config get backends` reads them so an option's dump
/// carries the same fields and defaults as the type's model.
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

  /// A `float` field; `default` is its default.
  pub const fn float_field(name: &'static str, default: OptionFieldDefault) -> OptionField {
    OptionField { name, default, kind: OptionFieldKind::Float }
  }
}

/// What the registered packages add to the config schema. `charliebot-registry` builds it at startup.
#[derive(Clone, Default)]
pub struct ConfigSchema {
  pub sections: Vec<ConfigSection>,
  /// Retired top-level keys that no section owns.
  pub legacy_keys: Vec<(&'static str, &'static str)>,
  /// The registered backend type names.
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
  /// Session cgroups carry a cpu weight; false stops the cpu controller and weight writes.
  #[serde(default = "default_switch_on")]
  pub enable_session_cpu_weight: bool,
  /// Session cgroups carry an io weight once a host turns this on; the default false stops the io controller and
  /// weight writes. The io controller reaches the agents slice only after `scripts/systemd/setup-io-share.sh` has
  /// delegated io there: a host sets this true when io appears in the slice's `cgroup.controllers`.
  #[serde(default = "default_switch_off")]
  pub enable_session_io_weight: bool,
  /// Agent runs carry the local work guard: a compile or test run on this host is refused, and the refusal names the
  /// build host that `remote_run.host` sets. false sets none of the guard's variables.
  #[serde(default = "default_switch_on")]
  pub enable_local_work_guard: bool,
  #[serde(default = "default_min_free_disk_gib")]
  pub min_free_disk_gib: i64,
  /// The address a person reaches this server at, e.g. `https://<host>.<tailnet>:18498`. It has no default: a guessed
  /// address would send links to the wrong place, so an unset value stays unset until the host sets it.
  #[serde(default)]
  pub public_url: Option<String>,
}

/// `remote_run:` section: the build host that `dev/remote-run` sends runs to, and the state directory it uses there. A
/// top-level section, not `server:` keys, because a live release refuses unknown `server:` keys: a host sets this
/// section while the previous release still runs. The runner and the local work guard's refusal read these keys on
/// each run, so an edit reaches the next run without a server restart.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteRunConfig {
  /// The ssh alias of the build host, as `~/.ssh/config` names it. It has no default: until the host sets it, the
  /// runner stops with exit 2 and names this key.
  #[serde(default)]
  pub host: Option<String>,
  /// The runner's state directory on the build host, relative to its home.
  #[serde(default = "default_remote_base")]
  pub remote_base: String,
}

impl Default for RemoteRunConfig {
  fn default() -> Self {
    Self { host: None, remote_base: default_remote_base() }
  }
}

fn default_remote_base() -> String {
  "scripts/remote-run".to_string()
}

// The schema defaults of `ServerConfig`.
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
fn default_switch_on() -> bool {
  true
}
fn default_switch_off() -> bool {
  false
}

/// The two session-weight switches of a parsed `config.yaml` as `(cpu, io)`. An absent switch reads as its
/// `ServerConfig` default, the value the server starts with, so every reader of a home's switches goes through here.
pub fn session_weight_switches(document: &serde_json::Value) -> Result<(bool, bool), serde_json::Error> {
  let server = document.get("server").cloned().unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
  let server: ServerConfig = serde_json::from_value(server)?;
  Ok((server.enable_session_cpu_weight, server.enable_session_io_weight))
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
  /// How the model's account is paid for: `billing: subscription | payg`. A priority pool's member needs it; every pool
  /// that names this option reads it here.
  pub billing: Option<PoolBilling>,
  /// The registered backend type, one of [`ConfigSchema::backend_types`].
  pub backend_type: String,
  /// The type-specific keys, raw until the backend package parses them.
  pub type_fields: Mapping,
  /// Whether the web pickers list the entry (`visible: true`, the default). A hidden entry keeps its
  /// label everywhere a label renders and still serves pool membership, `backends.calls` targets and
  /// the switch route; only what a picker offers changes.
  pub visible: bool,
}

impl BackendOption {
  /// The option's configured model, or the refusal for an option that leaves it empty.
  ///
  /// `subject` names what needs the model, and the refusal reads `<subject> '<id>' has no default model`.
  pub fn required_model(&self, subject: &str) -> Result<String, String> {
    if self.model.is_empty() {
      return Err(format!("{subject} '{}' has no default model", self.id));
    }
    Ok(self.model.clone())
  }
}

/// `backends.routing:` section: the host command that orders unpinned backend candidates.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingConfig {
  pub command: Vec<String>,
}

/// The word a `backends.calls` key takes to mean the session that starts the call. It shares the id namespace with
/// `backends.options` and `backends.pools`, so neither may use it as an id.
pub const SESSION_CALL_TARGET: &str = "session";

/// How a pool picks the member a call runs on: `rule: priority | rotate` in `backends.pools[]`.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PoolRule {
  /// The best selectable member: subscriptions below the 90% line, then all subscriptions as a last resort; the
  /// active sessions each member holds order each subscription tier, with configured order breaking ties.
  /// Pay-as-you-go follows only when no subscription is selectable.
  Priority,
  /// The first selectable member after the pool's rotation position, which then moves past the chosen member.
  Rotate,
}

/// How a `backends.options` entry is paid for: `billing: subscription | payg`.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PoolBilling {
  /// A subscription seat with quota windows.
  Subscription,
  /// Pay-as-you-go credit.
  Payg,
}

impl PoolBilling {
  /// The value the config writes for this billing.
  pub const fn key(self) -> &'static str {
    match self {
      PoolBilling::Subscription => "subscription",
      PoolBilling::Payg => "payg",
    }
  }
}

/// One member of a pool: the `backends.options` id it names. A pool's rule does not change the member's shape; its
/// billing is the option's ([`BackendOption::billing`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolMember {
  pub backend: String,
}

/// One `backends.pools[]` entry: a named set of members and the rule that picks one of them for a call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolConfig {
  pub id: String,
  pub label: String,
  pub rule: PoolRule,
  /// The members in config order. A rotate pool may name one option twice; the repeat takes a larger share of calls.
  pub members: Vec<PoolMember>,
  /// Whether the web pickers list the pool (`visible: true`, the default). See [`BackendOption::visible`].
  pub visible: bool,
}

/// One internal call class that `backends.calls` configures.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CallClass {
  Worker,
  Improve,
  Verify,
  Review,
  ColdRead,
  Probe,
  Autoname,
  Triage,
  Thread,
  NewSession,
}

impl CallClass {
  /// Every call class, in the order the `backends.calls` table lists them.
  pub const ALL: [CallClass; 10] = [
    CallClass::Worker,
    CallClass::Improve,
    CallClass::Verify,
    CallClass::Review,
    CallClass::ColdRead,
    CallClass::Probe,
    CallClass::Autoname,
    CallClass::Triage,
    CallClass::Thread,
    CallClass::NewSession,
  ];

  /// The `backends.calls` key that configures this class.
  pub const fn key(self) -> &'static str {
    match self {
      CallClass::Worker => "worker",
      CallClass::Improve => "improve",
      CallClass::Verify => "verify",
      CallClass::Review => "review",
      CallClass::ColdRead => "cold_read",
      CallClass::Probe => "probe",
      CallClass::Autoname => "autoname",
      CallClass::Triage => "triage",
      CallClass::Thread => "thread",
      CallClass::NewSession => "new_session",
    }
  }

  /// Whether the key accepts `session`: the five classes a session starts and that therefore have one to name.
  pub const fn allows_session(self) -> bool {
    matches!(self, CallClass::Worker | CallClass::Improve | CallClass::Verify | CallClass::Review | CallClass::ColdRead)
  }
}

/// What one `backends.calls` key names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallTarget {
  /// `session`: the session that starts the call runs it on that session's own backend.
  Session,
  /// A `backends.options` id or a `backends.pools` id.
  Backend(String),
}

/// `backends.calls:` table: the model or pool each internal call class runs on. A class without a key keeps the
/// behavior that config without a pool for that class gives.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CallsConfig {
  targets: HashMap<CallClass, CallTarget>,
}

impl CallsConfig {
  /// The target the config names for `class`, or `None` when the key is unset.
  pub fn get(&self, class: CallClass) -> Option<&CallTarget> {
    self.targets.get(&class)
  }
}

impl FromIterator<(CallClass, CallTarget)> for CallsConfig {
  fn from_iter<I: IntoIterator<Item = (CallClass, CallTarget)>>(targets: I) -> Self {
    Self { targets: targets.into_iter().collect() }
  }
}

/// `backends:` section: the router command, the model options, the named pools, and the call targets.
#[derive(Debug, Clone, PartialEq)]
pub struct BackendsConfig {
  pub routing: Option<RoutingConfig>,
  pub options: Vec<BackendOption>,
  pub pools: Vec<PoolConfig>,
  pub calls: CallsConfig,
}

impl BackendsConfig {
  /// The pool named `id`, or `None` when no `backends.pools` entry has that id.
  pub fn pool(&self, id: &str) -> Option<&PoolConfig> {
    self.pools.iter().find(|pool| pool.id == id)
  }

  /// The entries every web picker lists, as `(id, label)` pairs: the visible `backends.options` in config order,
  /// then the visible `backends.pools` in config order.
  pub fn picker_entries(&self) -> Vec<(String, String)> {
    self
      .options
      .iter()
      .filter(|option| option.visible)
      .map(|option| (option.id.clone(), option.label.clone()))
      .chain(self.pools.iter().filter(|pool| pool.visible).map(|pool| (pool.id.clone(), pool.label.clone())))
      .collect()
  }

  /// Whether the id names a `backends.options` or `backends.pools` entry a picker lists. A hidden entry
  /// answers false; a session's own active entry stays exempt, so the header menu still shows it.
  pub fn picker_visible(&self, id: &str) -> bool {
    self.options.iter().any(|option| option.id == id && option.visible)
      || self.pools.iter().any(|pool| pool.id == id && pool.visible)
  }

  /// The backend a session or scheduled task takes when its request names none: the `backends.calls.new_session`
  /// target, else the first `backends.options` id, else `None` when neither is configured.
  pub fn new_session_backend(&self) -> Option<String> {
    match self.calls.get(CallClass::NewSession) {
      Some(CallTarget::Backend(backend_id)) => Some(backend_id.clone()),
      Some(CallTarget::Session) => unreachable!("the config loader refuses session on backends.calls.new_session"),
      None => self.options.first().map(|option| option.id.clone()),
    }
  }
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

/// `github:` section: the operator's own GitHub token command and the agent `gh` wrapper's App token command, `~`
/// expanded.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubConfig {
  /// The command line whose standard output is the operator's own GitHub token. The draft sender runs it without a
  /// shell. Absent, a GitHub draft cannot be sent from the operator's account.
  #[serde(default)]
  pub user_token_command: Option<String>,
  /// The executable the wrapper calls with `--write <owner>/<repo>`. Absent, the wrapper refuses every
  /// text-carrying write.
  #[serde(default)]
  pub app_token_command: Option<String>,
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
  pub remote_run: RemoteRunConfig,
  pub github: GithubConfig,
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

/// The retired top-level key `key` moved to, as the loader prints it, or `None` for a live key.
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
        "{config_path_text} still uses retired keys ({}); declare each set as a backends.pools entry (id, label, \
         rule, members) and name each call class in backends.calls",
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

  let mut github: GithubConfig = typed_section(&config_path_text, "github", section("github"), GithubConfig::default)?;
  github.user_token_command = github.user_token_command.as_deref().map(paths::expanduser);
  github.app_token_command = github.app_token_command.as_deref().map(paths::expanduser);

  Ok(CharlieBotConfig {
    charliebot_home: home.to_path_buf(),
    headless_chrome_bin,
    server: typed_section(&config_path_text, "server", section("server"), default_server)?,
    paths: paths_config,
    backends: parse_backends(&config_path_text, section("backends"), &schema.backend_types)?,
    ui: typed_section(&config_path_text, "ui", section("ui"), || UiConfig { home_services: Vec::new() })?,
    telegram: typed_section(&config_path_text, "telegram", section("telegram"), || TelegramConfig { chat_id: None })?,
    remote_run: typed_section(&config_path_text, "remote_run", section("remote_run"), RemoteRunConfig::default)?,
    github,
    sections: Sections(sections),
  })
}

/// Refuse a config with no `backends.options` entry: the server runs no backend without one. The id and reference
/// rules (`check_pools`, `parse_calls`) run at load, so a config that loads names only options, pools and `session`.
pub fn require_backends(cfg: &CharlieBotConfig) -> Result<(), ConfigError> {
  if cfg.backends.options.is_empty() {
    return Err(ConfigError(
      "config.yaml: backends.options lists no backend; copy the starter entries from configs/config.example.yaml"
        .to_string(),
    ));
  }
  Ok(())
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
    enable_session_cpu_weight: default_switch_on(),
    enable_session_io_weight: default_switch_off(),
    enable_local_work_guard: default_switch_on(),
    min_free_disk_gib: default_min_free_disk_gib(),
    public_url: None,
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
    Some(value) => deserialize_at(config_path, name, value),
  }
}

/// Deserialize `value` as the config key `name`; a failure names the key and the path inside it.
fn deserialize_at<T: serde::de::DeserializeOwned>(
  config_path: &str,
  name: &str,
  value: Value,
) -> Result<T, ConfigError> {
  serde_path_to_error::deserialize(value).map_err(|error| {
    let location = error.path().to_string();
    let key = match location.as_str() {
      "." => name.to_string(),
      indexed if indexed.starts_with('[') => format!("{name}{indexed}"),
      nested => format!("{name}.{nested}"),
    };
    ConfigError(format!("{config_path}: invalid config key {key}: {}", error.into_inner()))
  })
}

fn parse_backends(
  config_path: &str,
  value: Option<Value>,
  backend_types: &[&str],
) -> Result<BackendsConfig, ConfigError> {
  let mapping = match value {
    None => {
      return Ok(BackendsConfig {
        routing: None,
        options: Vec::new(),
        pools: Vec::new(),
        calls: CallsConfig::default(),
      });
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
    if !matches!(key.as_str(), Some("routing" | "options" | "pools" | "calls")) {
      let shown = key.as_str().map_or_else(|| yaml_key_repr(key), str::to_string);
      return Err(ConfigError(format!("{config_path}: invalid config key backends.{shown}: unknown field")));
    }
  }
  let routing: Option<RoutingConfig> =
    typed_section(config_path, "backends.routing", mapping.get("routing").cloned(), || None)?;
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
  let pools = parse_pools(config_path, mapping.get("pools"))?;
  check_pools(config_path, &pools, &options)?;
  let calls = parse_calls(config_path, mapping.get("calls"), &pools, &options)?;
  Ok(BackendsConfig { routing, options, pools, calls })
}

/// The `backends.pools` list. Each member is a `backends.options` id under either rule.
fn parse_pools(config_path: &str, value: Option<&Value>) -> Result<Vec<PoolConfig>, ConfigError> {
  let Some(value) = value else {
    return Ok(Vec::new());
  };
  let Value::Sequence(entries) = value else {
    return Err(ConfigError(format!(
      "{config_path}: invalid config key backends.pools: expected a list, got {}",
      yaml_value_kind(value)
    )));
  };
  entries.iter().enumerate().map(|(index, entry)| parse_pool(config_path, index, entry)).collect()
}

/// One `backends.pools[]` entry: its id, label and rule, and its members as option ids.
fn parse_pool(config_path: &str, index: usize, entry: &Value) -> Result<PoolConfig, ConfigError> {
  #[derive(Deserialize)]
  #[serde(deny_unknown_fields)]
  struct PoolEntry {
    id: String,
    label: String,
    rule: PoolRule,
    members: Vec<Value>,
    /// Left out: the pool lists in the web pickers. A non-boolean refuses through serde's type check.
    #[serde(default)]
    visible: Option<bool>,
  }

  let location = format!("backends.pools[{index}]");
  let PoolEntry { id, label, rule, members, visible } = deserialize_at(config_path, &location, entry.clone())?;
  let visible = visible.unwrap_or(true);
  let members = members
    .iter()
    .enumerate()
    .map(|(member_index, member)| match member.as_str() {
      Some(backend) => Ok(PoolMember { backend: backend.to_string() }),
      None => Err(ConfigError(format!(
        "{config_path}: invalid config key {location}.members[{member_index}]: a pool member is a backends.options id, \
         got {}",
        yaml_value_kind(member)
      ))),
    })
    .collect::<Result<Vec<_>, _>>()?;
  Ok(PoolConfig { id, label, rule, members, visible })
}

/// Refuse pools that break the id rules, name a member that is not a `backends.options` id, or name a billingless
/// option on a priority pool. Options ids, pool ids and the word `session` share one namespace, so each id is unique
/// across them. The error names the entry.
fn check_pools(config_path: &str, pools: &[PoolConfig], options: &[BackendOption]) -> Result<(), ConfigError> {
  let mut names = HashSet::new();
  for (index, option) in options.iter().enumerate() {
    if option.id == SESSION_CALL_TARGET {
      return Err(ConfigError(format!(
        "{config_path}: backends.options[{index}] uses the reserved id '{SESSION_CALL_TARGET}'; it names the session \
         that starts a call, not a backend"
      )));
    }
    if !names.insert(option.id.as_str()) {
      return Err(ConfigError(format!("{config_path}: backends.options[{index}] repeats id '{}'", option.id)));
    }
  }
  for (index, pool) in pools.iter().enumerate() {
    let location = format!("{config_path}: backends.pools[{index}]");
    if pool.id == SESSION_CALL_TARGET {
      return Err(ConfigError(format!(
        "{location}: id '{SESSION_CALL_TARGET}' is reserved for the session that starts a call"
      )));
    }
    if !names.insert(pool.id.as_str()) {
      let kind =
        if options.iter().any(|option| option.id == pool.id) { "a backends.options id" } else { "another pool id" };
      return Err(ConfigError(format!("{location}: id '{}' is already {kind}", pool.id)));
    }
    if pool.members.is_empty() {
      return Err(ConfigError(format!("{location}.members lists no member")));
    }
    let mut seen = HashSet::new();
    for (member_index, member) in pool.members.iter().enumerate() {
      let member_location = format!("{location}.members[{member_index}]");
      if pools.iter().any(|other| other.id == member.backend) {
        return Err(ConfigError(format!(
          "{member_location}: '{}' is a pool; a pool member must be a backends.options id",
          member.backend
        )));
      }
      let Some(option) = options.iter().find(|option| option.id == member.backend) else {
        return Err(ConfigError(format!(
          "{member_location}: backend '{}' is not a backends.options id",
          member.backend
        )));
      };
      if pool.rule == PoolRule::Priority && option.billing.is_none() {
        return Err(ConfigError(format!(
          "{member_location}: backend '{}' has no billing; priority pool '{}' needs billing: subscription or payg on \
           its backends.options entry",
          member.backend, pool.id
        )));
      }
      if pool.rule == PoolRule::Priority && !seen.insert(member.backend.as_str()) {
        return Err(ConfigError(format!("{member_location}: backend '{}' appears twice", member.backend)));
      }
    }
  }
  Ok(())
}

/// The `backends.calls` table: each key names a call class, and each value names a model or pool, or `session` on a
/// class that a session starts. An unknown key, an unknown value, or `session` on another class refuses the file.
fn parse_calls(
  config_path: &str,
  value: Option<&Value>,
  pools: &[PoolConfig],
  options: &[BackendOption],
) -> Result<CallsConfig, ConfigError> {
  let Some(value) = value else {
    return Ok(CallsConfig::default());
  };
  let Value::Mapping(mapping) = value else {
    return Err(ConfigError(format!(
      "{config_path}: invalid config key backends.calls: expected a mapping, got {}",
      yaml_value_kind(value)
    )));
  };
  let mut targets = HashMap::new();
  for (key, entry) in mapping {
    let name = key.as_str().map_or_else(|| yaml_key_repr(key), str::to_string);
    let class = CallClass::ALL.into_iter().find(|class| class.key() == name).ok_or_else(|| {
      let keys = CallClass::ALL.map(CallClass::key).join(", ");
      ConfigError(format!(
        "{config_path}: invalid config key backends.calls.{name}: unknown call class; the keys are {keys}"
      ))
    })?;
    let text = entry.as_str().ok_or_else(|| {
      ConfigError(format!(
        "{config_path}: invalid config key backends.calls.{name}: expected a model or pool id, got {}",
        yaml_value_kind(entry)
      ))
    })?;
    let target = if text == SESSION_CALL_TARGET {
      if !class.allows_session() {
        return Err(ConfigError(format!(
          "{config_path}: invalid config key backends.calls.{name}: '{SESSION_CALL_TARGET}' is allowed only on \
           worker, improve, verify, review and cold_read; name a model or pool id"
        )));
      }
      CallTarget::Session
    } else if pools.iter().any(|pool| pool.id == text) || options.iter().any(|option| option.id == text) {
      CallTarget::Backend(text.to_string())
    } else {
      return Err(ConfigError(format!(
        "{config_path}: invalid config key backends.calls.{name}: '{text}' is not a backends.options or backends.pools \
         id"
      )));
    };
    targets.insert(class, target);
  }
  Ok(targets.into_iter().collect())
}

/// One backend entry, validated as the option model its `type` names would (`parse_option`).
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
        Some(Value::String(name)) => text_repr::str_repr(name),
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
  let billing: Option<PoolBilling> = match entry.get("billing") {
    None | Some(Value::Null) => None,
    Some(value) => Some(deserialize_at(config_path, &format!("backends.options[{index}].billing"), value.clone())?),
  };
  let visible = match entry.get("visible") {
    None | Some(Value::Null) => true,
    Some(Value::Bool(flag)) => *flag,
    Some(other) => {
      return Err(ConfigError(format!("{location}.visible: expected a boolean, got {}", yaml_value_kind(other))));
    }
  };
  let type_fields: Mapping = entry
    .iter()
    .filter(|(key, _)| {
      !matches!(key.as_str(), Some("id" | "label" | "model" | "prompt_overlay" | "billing" | "visible" | "type"))
    })
    .map(|(key, value)| (key.clone(), value.clone()))
    .collect();
  Ok(BackendOption { id, label, model, prompt_overlay, billing, backend_type, type_fields, visible })
}

/// A YAML mapping key as its repr prints the value loaded for it.
pub fn yaml_key_repr(key: &Value) -> String {
  match key {
    Value::String(text) => text_repr::str_repr(text),
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

  const VALID: &str = "server:\n  host: 127.0.0.1\n  port: 18650\nbackends:\n  pools:\n    - {id: pool, label: Pool, \
                       rule: rotate, members: [claude-fable]}\n  options:\n    - id: \
                       claude-fable\n      label: \"CC · Fable\"\n      type: cc-claude\n      model: claude-fable-5-1\n      \
                       effort: xhigh\nvoice: {}\n";

  #[test]
  fn a_valid_file_parses_into_typed_sections() {
    let home = home_with(VALID);
    let cfg = load(&home).unwrap();
    assert_eq!(cfg.server.port, 18650);
    assert_eq!(cfg.server.session_memory_max_mb, 12288);
    assert!(cfg.server.enable_session_cpu_weight);
    assert!(!cfg.server.enable_session_io_weight);
    assert!(cfg.server.enable_local_work_guard);
    assert_eq!(cfg.remote_run, RemoteRunConfig { host: None, remote_base: "scripts/remote-run".to_string() });
    assert_eq!(member_ids(cfg.backends.pool("pool").unwrap()), ["claude-fable"]);
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
  fn session_weight_switches_take_their_explicit_values() {
    let home = home_with("server:\n  enable_session_cpu_weight: false\n  enable_session_io_weight: false\nvoice: {}\n");
    let cfg = load(&home).unwrap();
    assert!(!cfg.server.enable_session_cpu_weight);
    assert!(!cfg.server.enable_session_io_weight);
    let home = home_with("server:\n  enable_session_io_weight: true\nvoice: {}\n");
    let cfg = load(&home).unwrap();
    assert!(cfg.server.enable_session_io_weight);
  }

  #[test]
  fn the_local_work_guard_switch_and_the_runner_section_parse() {
    let home = home_with(
      "server:\n  enable_local_work_guard: false\nremote_run:\n  host: build-host\n  remote_base: scripts/remote-run\n\
       voice: {}\n",
    );
    let cfg = load(&home).unwrap();
    assert!(!cfg.server.enable_local_work_guard);
    assert_eq!(cfg.remote_run.host.as_deref(), Some("build-host"));
    assert_eq!(cfg.remote_run.remote_base, "scripts/remote-run");
  }

  #[test]
  fn an_unknown_remote_run_key_names_the_section() {
    let home = home_with("remote_run:\n  base: scripts/remote-run\nvoice: {}\n");
    let error = load(&home).unwrap_err().to_string();
    assert!(error.contains("remote_run") && error.contains("unknown field"), "{error}");
  }

  #[test]
  fn a_backend_option_without_a_model_reports_its_subject_and_id() {
    let option = BackendOption {
      id: "opencode".to_string(),
      label: "OpenCode".to_string(),
      model: String::new(),
      prompt_overlay: None,
      billing: None,
      backend_type: "opencode".to_string(),
      type_fields: Mapping::new(),
      visible: true,
    };
    assert_eq!(option.required_model("backend").unwrap_err(), "backend 'opencode' has no default model");
  }

  /// The VALID config with one option and one pool hidden from the pickers.
  const VISIBILITY: &str = "\
server:
  host: 127.0.0.1
  port: 18650
backends:
  pools:
    - {id: pool, label: Pool, rule: rotate, members: [claude-fable], visible: false}
    - {id: shown, label: Shown, rule: rotate, members: [claude-fable]}
  options:
    - id: claude-fable
      label: \"CC · Fable\"
      type: cc-claude
      model: claude-fable-5-1
      visible: false
    - id: codex-fake
      label: Codex
      type: codex
      model: codex-model
voice: {}
";

  #[test]
  fn visible_defaults_to_true_and_the_pickers_drop_the_hidden_entries() {
    let cfg = load(&home_with(VISIBILITY)).unwrap();
    assert!(!cfg.get_backend_option("claude-fable").unwrap().visible);
    assert!(cfg.get_backend_option("codex-fake").unwrap().visible);
    assert!(!cfg.backends.pool("pool").unwrap().visible);
    assert!(cfg.backends.pool("shown").unwrap().visible);
    assert_eq!(
      cfg.backends.picker_entries(),
      vec![("codex-fake".to_string(), "Codex".to_string()), ("shown".to_string(), "Shown".to_string())]
    );
    assert!(!cfg.backends.picker_visible("claude-fable"));
    assert!(!cfg.backends.picker_visible("pool"));
    assert!(cfg.backends.picker_visible("shown"));
    // The visibility switch stays a shared field: it never lands in the type's own fields.
    assert!(!cfg.get_backend_option("claude-fable").unwrap().type_fields.contains_key("visible"));
  }

  #[test]
  fn a_config_without_the_field_lists_every_entry_in_the_pickers() {
    let cfg = load(&home_with(VALID)).unwrap();
    assert!(cfg.get_backend_option("claude-fable").unwrap().visible);
    assert!(cfg.backends.pool("pool").unwrap().visible);
    assert_eq!(
      cfg.backends.picker_entries(),
      vec![("claude-fable".to_string(), "CC · Fable".to_string()), ("pool".to_string(), "Pool".to_string())]
    );
    assert!(cfg.backends.picker_visible("claude-fable") && cfg.backends.picker_visible("pool"));
  }

  #[test]
  fn a_visible_value_that_is_not_a_boolean_refuses_the_option_and_the_pool() {
    let home = home_with(&VISIBILITY.replace("      visible: false", "      visible: certainly"));
    let error = load(&home).unwrap_err().to_string();
    assert!(error.contains("backends.options[0].visible: expected a boolean, got a string"), "{error}");

    let home = home_with(
      &VISIBILITY.replace("members: [claude-fable], visible: false}", "members: [claude-fable], visible: [true]}"),
    );
    let error = load(&home).unwrap_err().to_string();
    assert!(error.contains("backends.pools[0].visible") && error.contains("expected a boolean"), "{error}");
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
  fn github_app_token_command_is_optional_and_expands_a_configured_tilde() {
    let cfg = load(&home_with(VALID)).unwrap();
    assert_eq!(cfg.github.app_token_command, None);

    let config_text = format!("{VALID}github:\n  app_token_command: ~/synthetic/app-token-command\n");
    let cfg = load(&home_with(&config_text)).unwrap();
    let expected = paths::expanduser("~/synthetic/app-token-command");
    assert_eq!(cfg.github.app_token_command.as_deref(), Some(expected.as_str()));

    let home = home_with(&format!("{VALID}github:\n  bogus: x\n"));
    let error = load(&home).unwrap_err().to_string();
    assert!(error.contains("invalid config key github.bogus"), "{error}");
  }

  #[test]
  fn github_user_token_command_is_optional_and_expands_a_configured_tilde() {
    let cfg = load(&home_with(VALID)).unwrap();
    assert_eq!(cfg.github.user_token_command, None);

    let config_text = format!("{VALID}github:\n  user_token_command: ~/synthetic/user-token-command\n");
    let cfg = load(&home_with(&config_text)).unwrap();
    let expected = paths::expanduser("~/synthetic/user-token-command");
    assert_eq!(cfg.github.user_token_command.as_deref(), Some(expected.as_str()));
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
  fn a_leftover_strong_key_is_refused_by_name() {
    let home = home_with(
      "backends:\n  strong: synthetic-strong\n  options:\n    - {id: synthetic-strong, label: S, type: codex, model: m}\n",
    );
    let error = load(&home).unwrap_err().to_string();
    assert!(error.contains("invalid config key backends.strong") && error.contains("unknown field"), "{error}");
  }

  fn member_ids(pool: &PoolConfig) -> Vec<&str> {
    pool.members.iter().map(|member| member.backend.as_str()).collect()
  }

  const TWO_OPTIONS: &str = concat!(
    "backends:\n  options:\n",
    "    - {id: a, label: A, type: codex, model: m}\n",
    "    - {id: b, label: B, type: codex, model: m}\n",
  );

  const TWO_BILLED_OPTIONS: &str = concat!(
    "backends:\n  options:\n",
    "    - {id: a, label: A, type: codex, model: m, billing: subscription}\n",
    "    - {id: b, label: B, type: codex, model: m, billing: payg}\n",
  );

  fn billing_of(cfg: &CharlieBotConfig, id: &str) -> Option<PoolBilling> {
    cfg.get_backend_option(id).and_then(|option| option.billing)
  }

  #[test]
  fn the_retired_keys_fail_loading_naming_the_pools_form() {
    let home = home_with(&format!("{TWO_OPTIONS}  pool: [a]\n"));
    let error = load(&home).unwrap_err().to_string();
    assert!(
      error.contains("backends.pool") && error.contains("backends.pools") && error.contains("backends.calls"),
      "{error}"
    );

    let home = home_with(&format!(
      "{TWO_OPTIONS}  auto:\n    label: Auto\n    members:\n      - {{backend: a, billing: subscription}}\n"
    ));
    let error = load(&home).unwrap_err().to_string();
    assert!(
      error.contains("backends.auto") && error.contains("backends.pools") && error.contains("backends.calls"),
      "{error}"
    );
  }

  #[test]
  fn the_named_form_parses_pools_rules_and_call_targets() {
    let home = home_with(
      &[
        TWO_BILLED_OPTIONS,
        concat!(
          "  pools:\n",
          "    - {id: auto, label: Auto, rule: priority, members: [a, b]}\n",
          "    - {id: light, label: Light, rule: rotate, members: [b, a, b]}\n",
          "  calls:\n",
          "    worker: session\n",
          "    verify: light\n",
          "    thread: auto\n",
          "    new_session: a\n",
        ),
      ]
      .concat(),
    );
    let cfg = load(&home).unwrap();
    assert_eq!(cfg.backends.pools.iter().map(|pool| pool.id.as_str()).collect::<Vec<_>>(), ["auto", "light"]);
    assert_eq!(member_ids(cfg.backends.pool("auto").unwrap()), ["a", "b"]);
    assert_eq!(member_ids(cfg.backends.pool("light").unwrap()), ["b", "a", "b"]);
    assert_eq!(billing_of(&cfg, "a"), Some(PoolBilling::Subscription));
    assert_eq!(billing_of(&cfg, "b"), Some(PoolBilling::Payg));
    assert_eq!(cfg.backends.calls.get(CallClass::Worker), Some(&CallTarget::Session));
    assert_eq!(cfg.backends.calls.get(CallClass::Verify), Some(&CallTarget::Backend("light".to_string())));
    assert_eq!(cfg.backends.calls.get(CallClass::NewSession), Some(&CallTarget::Backend("a".to_string())));
    assert_eq!(cfg.backends.calls.get(CallClass::Probe), None, "an unset key stays unset");
  }

  #[test]
  fn a_pool_member_that_is_a_pool_id_fails_loading() {
    let home = home_with(
      &[
        TWO_BILLED_OPTIONS,
        concat!(
          "  pools:\n",
          "    - {id: auto, label: Auto, rule: priority, members: [a]}\n",
          "    - {id: light, label: Light, rule: rotate, members: [auto]}\n",
        ),
      ]
      .concat(),
    );
    let error = load(&home).unwrap_err().to_string();
    assert!(
      error.contains("backends.pools[1].members[0]: 'auto' is a pool; a pool member must be a backends.options id"),
      "{error}"
    );
  }

  #[test]
  fn a_priority_member_whose_option_has_no_billing_fails_naming_the_pool_and_option() {
    let home =
      home_with(&format!("{TWO_OPTIONS}  pools:\n    - {{id: auto, label: Auto, rule: priority, members: [a]}}\n"));
    let error = load(&home).unwrap_err().to_string();
    assert!(
      error.contains(
        "backends.pools[0].members[0]: backend 'a' has no billing; priority pool 'auto' needs billing: subscription or \
         payg"
      ),
      "{error}"
    );
  }

  #[test]
  fn each_call_key_refuses_an_unknown_value_an_unknown_key_and_a_session_it_does_not_allow() {
    for (fragment, expected) in [
      (
        "  calls:\n    worker: ghost\n",
        "backends.calls.worker: 'ghost' is not a backends.options or backends.pools id",
      ),
      ("  calls:\n    speculate: a\n", "backends.calls.speculate: unknown call class"),
      ("  calls:\n    probe: session\n", "backends.calls.probe: 'session' is allowed only on"),
      ("  calls:\n    new_session: session\n", "backends.calls.new_session: 'session' is allowed only on"),
      ("  calls:\n    worker:\n", "backends.calls.worker: expected a model or pool id, got null"),
    ] {
      let error = load(&home_with(&format!("{TWO_OPTIONS}{fragment}"))).unwrap_err().to_string();
      assert!(error.contains(expected), "{expected}: {error}");
    }
  }

  #[test]
  fn each_malformed_pool_entry_refuses_by_name() {
    for (options, fragment, expected) in [
      (
        TWO_OPTIONS,
        "  pools:\n    - {id: p, label: P, rule: weekly, members: [a]}\n",
        "backends.pools[0].rule: unknown variant",
      ),
      (
        TWO_OPTIONS,
        "  pools:\n    - {id: p, label: P, rule: priority, members: [{backend: a, billing: payg}]}\n",
        "backends.pools[0].members[0]: a pool member is a backends.options id, got a mapping",
      ),
      (
        TWO_OPTIONS,
        "  pools:\n    - {id: p, label: P, rule: rotate, members: []}\n",
        "backends.pools[0].members lists no member",
      ),
      (
        TWO_BILLED_OPTIONS,
        "  pools:\n    - {id: p, label: P, rule: priority, members: [a, a]}\n",
        "backends.pools[0].members[1]: backend 'a' appears twice",
      ),
      (
        TWO_OPTIONS,
        "  pools:\n    - {id: p, label: P, rule: rotate, members: [ghost]}\n",
        "backends.pools[0].members[0]: backend 'ghost' is not a backends.options id",
      ),
      (
        TWO_OPTIONS,
        "  pools:\n    - {id: a, label: P, rule: rotate, members: [a]}\n",
        "id 'a' is already a backends.options id",
      ),
      (
        TWO_OPTIONS,
        "  pools:\n    - {id: session, label: P, rule: rotate, members: [a]}\n",
        "id 'session' is reserved",
      ),
      (
        TWO_OPTIONS,
        "  pools:\n    - {id: p, label: P, rule: rotate, members: [a]}\n    - {id: p, label: P, \
         rule: rotate, members: [a]}\n",
        "backends.pools[1]: id 'p' is already another pool id",
      ),
    ] {
      let error = load(&home_with(&format!("{options}{fragment}"))).unwrap_err().to_string();
      assert!(error.contains(expected), "{expected}: {error}");
    }
  }

  #[test]
  fn require_backends_refuses_a_config_with_no_option() {
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
  fn the_approval_policy_table_under_config_d_approval_d_is_not_a_stray_config_location() {
    let home = home_with(VALID);
    std::fs::create_dir_all(home.path().join("config.d/approval.d")).unwrap();
    std::fs::write(home.path().join("config.d/approval.d/policy.yaml"), "enable_ask_tier_refusal: false\n").unwrap();

    let cfg = load(&home).expect("the approval policy table is not a stray config file");

    assert_eq!(cfg.server.host, "127.0.0.1");
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
