//! The registry value and the typed contracts whose registrations it owns.

use std::any::{Any, TypeId, type_name};
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};
use std::time::Duration;

use anyhow::bail;
use axum::Router;
use axum::extract::ws::WebSocketUpgrade;
use axum::http::request::Parts;
use axum::response::Response;
use charliebot_core::RepoRoot;
use charliebot_core::config::{BackendOption, CharlieBotConfig, ConfigError, ConfigSchema, ConfigSection, OptionField};
use charliebot_core::credentials::Credentials;
use charliebot_core::hot_reload::HotReload;
use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot, MetadataSlotValidator};
use charliebot_core::paths;
use charliebot_store::metadata::{SessionMetadata, ThreadMetadata, UtcDateTime};
use charliebot_store::run_records::RunKind;
use charliebot_store::{SharedChatEvent, Store};
use futures_util::future::{BoxFuture, Shared};
use serde_json::{Map, Value};
use tokio::io::AsyncWrite;
use tokio::sync::watch;

use crate::route_claims::RouteClaims;
use crate::sequence::{SequenceBinding, SequenceController};
use crate::sidebar::SidebarContribution;
use crate::startup_stop::StartupStop;
use crate::usage::{QuotaAccount, SourceSweep, SweepScope, UsageSource};
/// A package's `register` function: the package's contributions, made through the registry's typed methods.
pub type RegisterFn = fn(&mut Registry) -> Result<(), RegistryError>;

/// A registration that fails startup. Python raises `ValueError` at the same point; the message names the
/// package whose `register` made the registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryError {
  pub package: &'static str,
  pub message: String,
}

impl fmt::Display for RegistryError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "registering {}: {}", self.package, self.message)
  }
}

impl std::error::Error for RegistryError {}

/// One `register` step's result, a failure reported as `package`'s with the whole anyhow chain as the message. A step
/// that fails outside the registry — reading the host, opening a database — reaches the startup error this way.
pub fn registration_step<T>(package: &'static str, step: anyhow::Result<T>) -> Result<T, RegistryError> {
  step.map_err(|error| RegistryError { package, message: format!("{error:#}") })
}

/// One registration and the package that made it.
struct Entry<T> {
  key: &'static str,
  package: &'static str,
  value: T,
}

/// The registrations of one hook kind, in registration order. A name is unique within its kind.
pub(crate) struct Ledger<T> {
  /// The hook kind as an error message names it.
  kind: &'static str,
  entries: Vec<Entry<T>>,
}

impl<T> Ledger<T> {
  pub(crate) fn new(kind: &'static str) -> Ledger<T> {
    Ledger { kind, entries: Vec::new() }
  }

  /// Add a registration; a name that another registration of this kind holds fails with that package named.
  pub(crate) fn insert(&mut self, package: &'static str, key: &'static str, value: T) -> Result<(), RegistryError> {
    if let Some(existing) = self.entries.iter().find(|entry| entry.key == key) {
      return Err(RegistryError {
        package,
        message: format!("{} {key:?} is already registered by {}", self.kind, existing.package),
      });
    }
    self.entries.push(Entry { key, package, value });
    Ok(())
  }

  /// Add the one registration this kind allows; a second one fails with the first one's package named.
  pub(crate) fn insert_single(
    &mut self,
    package: &'static str,
    key: &'static str,
    value: T,
  ) -> Result<(), RegistryError> {
    if let Some(existing) = self.entries.first() {
      return Err(RegistryError {
        package,
        message: format!("the {} is already registered by {}", self.kind, existing.package),
      });
    }
    self.insert(package, key, value)
  }

  pub(crate) fn get(&self, key: &str) -> Option<&T> {
    self.entries.iter().find(|entry| entry.key == key).map(|entry| &entry.value)
  }

  /// Every value with its name, in registration order.
  pub(crate) fn pairs(&self) -> impl Iterator<Item = (&'static str, &T)> {
    self.entries.iter().map(|entry| (entry.key, &entry.value))
  }

  pub(crate) fn values(&self) -> impl Iterator<Item = &T> {
    self.entries.iter().map(|entry| &entry.value)
  }
}

fn append_ledger_inventory<T>(inventory: &mut Vec<String>, ledger: &Ledger<T>) {
  for entry in &ledger.entries {
    inventory.push(format!("{}\t{}\t{}", ledger.kind, entry.package, entry.key));
  }
}

/// What the registered packages contribute. Built at startup by [`Registry::register_package`] calls, then
/// shared read-only (`Arc<Registry>`); nothing registers after startup.
pub struct Registry {
  /// The package whose `register` is running. A registration outside one fails.
  pub(crate) current: Option<&'static str>,
  /// Every registered package in registration order. Each package's static files and templates join the pages.
  pub(crate) page_packages: Vec<&'static str>,
  pub(crate) routers: Vec<RouterRegistration>,
  pub(crate) websocket_routes: Ledger<WebSocketHandler>,
  pub(crate) services: Ledger<Arc<dyn Service>>,
  pub(crate) file_views: Ledger<Arc<dyn FileView>>,
  pub(crate) startup_checks: Ledger<StartupCheckFn>,
  pub(crate) config_checks: Ledger<CheckFn>,
  pub(crate) config_sections: Ledger<ConfigSection>,
  /// Every retired top-level key that packages registered, keyed by the retired name: the keys of
  /// registered sections and the keys registered on their own.
  pub(crate) config_legacy_keys: Ledger<&'static str>,
  pub(crate) diff_roots: Ledger<DiffRootFn>,
  pub(crate) template_globals: Ledger<TemplateGlobalFn>,
  pub(crate) home_cards: Vec<HomeCard>,
  pub(crate) scheduled_handlers: Ledger<ScheduledHandlerFn>,
  pub(crate) metadata_slots: Vec<MetadataSlot>,
  pub(crate) metadata_slot_validators: Vec<(&'static str, MetadataFile, MetadataSlotValidator)>,
  pub(crate) turn_contributions: Ledger<Arc<dyn TurnContribution>>,
  pub(crate) sidebar_contributions: Ledger<Arc<dyn SidebarContribution>>,
  pub(crate) usage_sources: Ledger<UsageSource>,
  /// Backend type name to the usage source its usage is attributed to.
  pub(crate) usage_attributions: Ledger<&'static str>,
  pub(crate) backend_types: Ledger<BackendTypeRegistration>,
  /// Each backend type's `backends.options[]` field schema, keyed by the type name: what
  /// `charliebot config get backends` renders beside the shared option fields.
  pub(crate) option_schemas: Ledger<&'static [OptionField]>,
  pub(crate) child_envs: Ledger<ChildEnvFn>,
  pub(crate) reading_limits: Ledger<ReadingLimitsFn>,
  pub(crate) backend_reading_limits: Vec<BackendReadingLimitsRegistration>,
  pub(crate) usage_resolvers: Ledger<UsageResolverConstructor>,
  pub(crate) sequence_controllers: Ledger<Arc<dyn SequenceController>>,
  /// The one reader of a session's plan registry, which `features/artifacts` registers.
  pub(crate) plan_registry: Ledger<PlanRegistryFn>,
  /// The one reader of every open session's Waiting on you items, which `features/web_reply` registers.
  pub(crate) open_waiting_items: Ledger<OpenWaitingItemsFn>,
  /// The one fork-assertion runner of a page, which `features/artifacts` registers.
  pub(crate) fork_assertions: Ledger<ForkAssertionsFn>,
  /// The lifecycle of a backend type that registered none: one process per run.
  pub(crate) default_lifecycle: Arc<dyn BackendLifecycle>,
  pub(crate) default_lifecycle_name: &'static str,
  pub(crate) pre_recovery_steps: Ledger<StepFn>,
  pub(crate) recovery_steps: Ledger<StepFn>,
  pub(crate) trigger_step: Ledger<StepFn>,
  pub(crate) let_go_steps: Ledger<StepFn>,
  /// The server's feature context, bound once right after the server builds it. The store's after-turn hooks read it
  /// through this link. It is a `Weak`: the store's hooks hold the registry, and a strong link would form a cycle that
  /// keeps the context alive.
  pub(crate) feature_context: OnceLock<Weak<FeatureContext>>,
}

impl Registry {
  /// Each registration as `<kind>\t<package>\t<key>`, in table and registration order.
  pub fn inventory(&self) -> Vec<String> {
    let Registry {
      current: _,
      page_packages,
      routers,
      websocket_routes,
      services,
      file_views,
      startup_checks,
      config_checks,
      config_sections,
      config_legacy_keys,
      diff_roots,
      template_globals,
      home_cards,
      scheduled_handlers,
      metadata_slots,
      metadata_slot_validators,
      turn_contributions,
      sidebar_contributions,
      usage_sources,
      usage_attributions,
      backend_types,
      option_schemas,
      child_envs,
      reading_limits,
      backend_reading_limits,
      usage_resolvers,
      sequence_controllers,
      plan_registry,
      open_waiting_items,
      fork_assertions,
      default_lifecycle: _,
      default_lifecycle_name,
      pre_recovery_steps,
      recovery_steps,
      trigger_step,
      let_go_steps,
      feature_context: _,
    } = self;

    let mut inventory = Vec::new();
    for &package in page_packages {
      inventory.push(format!("page_packages\t{package}\t{package}"));
    }
    for router in routers {
      inventory.push(format!("routers\t{}\t{}", router.package, router.package));
    }
    append_ledger_inventory(&mut inventory, websocket_routes);
    append_ledger_inventory(&mut inventory, services);
    append_ledger_inventory(&mut inventory, file_views);
    append_ledger_inventory(&mut inventory, startup_checks);
    append_ledger_inventory(&mut inventory, config_checks);
    append_ledger_inventory(&mut inventory, config_sections);
    append_ledger_inventory(&mut inventory, config_legacy_keys);
    append_ledger_inventory(&mut inventory, diff_roots);
    append_ledger_inventory(&mut inventory, template_globals);
    for card in home_cards {
      inventory.push(format!("home_cards\t-\t{}", card.name));
    }
    append_ledger_inventory(&mut inventory, scheduled_handlers);
    for slot in metadata_slots {
      inventory.push(format!("metadata_slots\t-\t{}:{:?}", slot.owner, slot.on));
    }
    for &(owner, on, _) in metadata_slot_validators {
      inventory.push(format!("metadata_slot_validators\t-\t{owner}:{on:?}"));
    }
    append_ledger_inventory(&mut inventory, turn_contributions);
    append_ledger_inventory(&mut inventory, sidebar_contributions);
    append_ledger_inventory(&mut inventory, usage_sources);
    append_ledger_inventory(&mut inventory, usage_attributions);
    append_ledger_inventory(&mut inventory, backend_types);
    append_ledger_inventory(&mut inventory, option_schemas);
    append_ledger_inventory(&mut inventory, child_envs);
    append_ledger_inventory(&mut inventory, reading_limits);
    for registration in backend_reading_limits {
      inventory.push(format!(
        "backend_reading_limits\t{}\t{}:{}",
        registration.package, registration.backend_type, registration.reading_kind
      ));
    }
    append_ledger_inventory(&mut inventory, usage_resolvers);
    append_ledger_inventory(&mut inventory, sequence_controllers);
    append_ledger_inventory(&mut inventory, plan_registry);
    append_ledger_inventory(&mut inventory, open_waiting_items);
    append_ledger_inventory(&mut inventory, fork_assertions);
    inventory.push(format!("default_lifecycle\t-\t{default_lifecycle_name}"));
    append_ledger_inventory(&mut inventory, pre_recovery_steps);
    append_ledger_inventory(&mut inventory, recovery_steps);
    append_ledger_inventory(&mut inventory, trigger_step);
    append_ledger_inventory(&mut inventory, let_go_steps);
    inventory
  }

  /// Run one package's `register` and list the package's page parts after those of the earlier packages.
  /// The composition root calls this for each package in order (`page_render.register_page_package`).
  pub fn register_package(&mut self, package: &'static str, register: RegisterFn) -> Result<(), RegistryError> {
    if self.page_packages.contains(&package) {
      return Err(RegistryError { package, message: "is listed more than once in the package list".to_string() });
    }
    self.current = Some(package);
    let result = register(self);
    self.current = None;
    result?;
    self.page_packages.push(package);
    Ok(())
  }

  /// The package whose registration is running. A registration outside `register_package` is a bug, so it fails.
  pub(crate) fn owner(&self) -> Result<&'static str, RegistryError> {
    self.current.ok_or(RegistryError {
      package: "(no package)",
      message: "a registration ran outside register_package".to_string(),
    })
  }

  /// Bind the server's feature context. The server calls this once, right after it builds the context; a second call
  /// fails and keeps the first binding.
  pub fn bind_feature_context(&self, features: &Arc<FeatureContext>) -> Result<(), StateError> {
    self
      .feature_context
      .set(Arc::downgrade(features))
      .map_err(|_| StateError("the registry already holds a FeatureContext".to_string()))
  }

  /// The bound feature context, or `None` when none is bound or the bound one was dropped.
  pub fn feature_context(&self) -> Option<Arc<FeatureContext>> {
    self.feature_context.get()?.upgrade()
  }

  /// The config schema the registered packages declare, for `charliebot_core::config::load_config`.
  pub fn config_schema(&self) -> ConfigSchema {
    ConfigSchema {
      sections: self.config_sections.values().copied().collect(),
      legacy_keys: self.config_legacy_keys.pairs().map(|(old, new)| (old, *new)).collect(),
      backend_types: self.backend_types.values().map(|registration| registration.type_name).collect(),
    }
  }

  /// Load `<home>/config.yaml` with the registered schema, then run each backend type's option model and each
  /// config check on it. A failure fails the load, so a hot reload keeps the previous config.
  pub fn load_config(&self, home: &Path) -> Result<CharlieBotConfig, ConfigError> {
    let cfg = charliebot_core::config::load_config(home, &self.config_schema())?;
    let config_file = paths::display(&cfg.config_file());
    for option in &cfg.backends.options {
      let registration =
        self.backend_types.get(&option.backend_type).expect("the loader accepts only registered backend types");
      (registration.option_model)(&option.type_fields).map_err(|message| {
        ConfigError(format!("{config_file}: backends.options: backend {:?}: {message}", option.id))
      })?;
    }
    for (_, check) in self.config_checks.pairs() {
      check(&cfg).map_err(ConfigError)?;
    }
    Ok(cfg)
  }

  /// The top-level config keys that packages own, with their parsers.
  pub fn config_sections(&self) -> impl Iterator<Item = &ConfigSection> {
    self.config_sections.values()
  }

  /// Register the top-level metadata keys one package owns in one file, in registration order. A second owner
  /// registration on one file fails (`metadata_slot_registration.register_metadata_fields`).
  pub fn metadata_slot(&mut self, slot: MetadataSlot) -> Result<(), RegistryError> {
    let package = self.owner()?;
    if self.metadata_slots.iter().any(|registered| registered.owner == slot.owner && registered.on == slot.on) {
      return Err(RegistryError {
        package,
        message: format!("metadata owner {:?} is already registered for {:?}", slot.owner, slot.on),
      });
    }
    self.metadata_slots.push(slot);
    Ok(())
  }

  /// Register one metadata slot with the validation the owner's Pydantic model applies to its values.
  pub fn metadata_slot_with_validator(
    &mut self,
    slot: MetadataSlot,
    validator: MetadataSlotValidator,
  ) -> Result<(), RegistryError> {
    let owner = slot.owner;
    let on = slot.on;
    self.metadata_slot(slot)?;
    self.metadata_slot_validators.push((owner, on, validator));
    Ok(())
  }

  /// The validator registered for one slot, if that package supplied one.
  pub fn metadata_slot_validator(&self, owner: &str, on: MetadataFile) -> Option<MetadataSlotValidator> {
    self
      .metadata_slot_validators
      .iter()
      .find(|(registered_owner, registered_on, _)| *registered_owner == owner && *registered_on == on)
      .map(|(_, _, validator)| *validator)
  }

  /// The metadata slots in registration order. The store serializes the owners' keys in this order.
  pub fn metadata_slots(&self) -> Vec<MetadataSlot> {
    self.metadata_slots.clone()
  }

  /// Register a config section's parser and its legacy keys (`config_registry.register_config_section`).
  pub fn config_section(&mut self, section: ConfigSection) -> Result<(), RegistryError> {
    let package = self.owner()?;
    self.config_sections.insert(package, section.key, section)?;
    self.legacy_keys(section.legacy_keys)
  }

  /// Register retired top-level keys that no section owns (`config_registry.register_legacy_keys`).
  pub fn legacy_keys(&mut self, legacy_keys: &[(&'static str, &'static str)]) -> Result<(), RegistryError> {
    let package = self.owner()?;
    for &(old, new) in legacy_keys {
      self.config_legacy_keys.insert(package, old, new)?;
    }
    Ok(())
  }

  /// Register a backend type's `backends.options[]` field schema: its type-specific fields in the Python
  /// option model's declaration order, with the value each holds when the entry omits it.
  ///
  /// `charliebot config get backends` renders an option from this schema, so the output carries the same
  /// fields and defaults as the type's pydantic model. A backend package registers its own schema next to
  /// its [`Registry::backend_type`] registration.
  pub fn option_schema(
    &mut self,
    backend_type: &'static str,
    schema: &'static [OptionField],
  ) -> Result<(), RegistryError> {
    let package = self.owner()?;
    self.option_schemas.insert(package, backend_type, schema)
  }

  /// The option schema `backend_type` registered, or `None` when the type registered none.
  pub fn registered_option_schema(&self, backend_type: &str) -> Option<&'static [OptionField]> {
    self.option_schemas.get(backend_type).copied()
  }

  /// Register a backend type's traits, option model, factory and lifecycle (`register_backend_type`).
  pub fn backend_type(&mut self, registration: BackendTypeRegistration) -> Result<(), RegistryError> {
    let package = self.owner()?;
    if registration.translate_fallback && self.backend_types.values().any(|known| known.translate_fallback) {
      return Err(RegistryError {
        package,
        message: format!(
          "backend type {:?} cannot serve the translate fallback: another type does",
          registration.type_name
        ),
      });
    }
    self.backend_types.insert(package, registration.type_name, registration)
  }
}

/// How a backend type resumes its conversation: a CLI flag, or the backend's own session id in its constructor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResumeStyle {
  CliFlag,
  NativeId,
}

/// The behaviors that runtime code reads by backend type (`backend_type_registration.BackendTraits`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BackendTraits {
  pub resume: ResumeStyle,
  /// True when a restarted server can follow the process of an interrupted run.
  pub restart_reattach: bool,
  /// True when the runtime chooses the session id of a task run before the process starts.
  pub preassigned_session_id: bool,
  /// The id prefix that names the type's family of backend ids; `None` when the type has no family.
  pub family_prefix: Option<&'static str>,
  /// True when the backend runs a `/compact` chat message as a manual compaction.
  pub manual_compact: bool,
  /// The compact button's title on a backend without manual compaction; `None` takes the page's generic title.
  pub compact_hint: Option<&'static str>,
}

/// Validates one `backends.options[]` entry's type-specific keys (the pydantic option model of Python).
pub type OptionModel = fn(&serde_yaml::Mapping) -> Result<(), String>;

/// One backend type (`backend_type_registration.register_backend_type`). `translate_fallback` marks the one type that
/// serves the binary-free translate fallback; its factory then also accepts no option.
pub struct BackendTypeRegistration {
  pub type_name: &'static str,
  pub traits: BackendTraits,
  pub option_model: OptionModel,
  pub factory: Arc<dyn BackendFactory>,
  /// `None` gives the default lifecycle: one process per run.
  pub lifecycle: Option<Arc<dyn BackendLifecycle>>,
  pub translate_fallback: bool,
}

/// The environment of an agent child process.
pub type Env = BTreeMap<String, String>;

/// Edits one agent child process's environment in place (`backend_lifecycle_registration.register_child_env`).
pub type ChildEnvFn = fn(&mut Env);

/// Read one session directory's plan registry: the `plans` list of its `plans.json`, or `None` when it has none.
pub type PlanRegistryFn = fn(&Path) -> anyhow::Result<Option<Vec<Value>>>;

/// One open session's Waiting on you item, as `features/web_reply` reports it to the approval view
/// (`status.OpenWaitingItem`). The registry carries the row between the two features.
#[derive(Clone, Debug)]
pub struct OpenWaitingRow {
  pub session_id: String,
  pub session_name: String,
  pub text: String,
  pub context: Option<String>,
  pub link: Option<String>,
  pub tier: Option<String>,
  pub rule: Option<String>,
  pub since: UtcDateTime,
}

/// Read every open session's Waiting on you items, the whole home's list (`status.open_waiting_items`).
pub type OpenWaitingItemsFn = for<'a> fn(&'a FeatureContext) -> BoxFuture<'a, anyhow::Result<Vec<OpenWaitingRow>>>;

/// Run one page's fork assertions, each failure as `(assertion name, detail)`. `None` means the page's file name
/// carries no genre prefix, so no assertion set applies to it.
pub type ForkAssertionsFn = fn(page: &Path, home: &Path) -> anyhow::Result<Option<Vec<(String, String)>>>;

/// The context limits of one reading kind. `declared_window` is `None` when the reading carries the window itself;
/// `compact_reserve` is `None` when the compaction point is unknown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContextLimits {
  pub declared_window: Option<u64>,
  pub compact_reserve: Option<u64>,
}

/// Computes the limits of one reading kind (`backend_lifecycle_registration.register_reading_limits`).
pub type ReadingLimitsFn = fn(RunKind) -> ContextLimits;

/// Computes a reading kind's limits from one backend option (`register_backend_reading_limits`).
pub type BackendReadingLimitsFn = fn(&BackendOption, RunKind) -> ContextLimits;

/// One context-limits provider registered for a backend type and reading kind.
pub(crate) struct BackendReadingLimitsRegistration {
  pub backend_type: &'static str,
  pub reading_kind: &'static str,
  pub package: &'static str,
  pub limits: BackendReadingLimitsFn,
}

/// Reads one backend type's usage from its session events (`register_usage_resolver`). A resolver returns the usage
/// object for the session, or `None`.
pub trait UsageResolver: Send + Sync {
  fn resolve(
    &self,
    session_id: &str,
    meta: &SessionMetadata,
    events: &[SharedChatEvent],
  ) -> anyhow::Result<Option<Map<String, Value>>>;
}

/// The server values a backend usage-resolver constructor can retain.
pub struct UsageResolverHost {
  pub config: Arc<HotReload<CharlieBotConfig>>,
  pub store: Arc<Store>,
}

/// Constructs a backend type's usage resolver for one usage owner.
pub type UsageResolverConstructor = Arc<dyn Fn(&UsageResolverHost) -> Arc<dyn UsageResolver> + Send + Sync>;

/// The keyword arguments the runtime passes to a backend factory (`base.py` launch arguments and the per-type
/// launch arguments).
#[derive(Clone, Debug, Default)]
pub struct LaunchArgs {
  pub extra_flags: Vec<String>,
  pub buffer_limit: Option<usize>,
  pub instructions_content: Option<String>,
  pub instructions_file: Option<PathBuf>,
  pub resume_session_id: Option<String>,
  pub log_dir: Option<PathBuf>,
  pub cgroup_session_id: Option<String>,
  /// The per-process arguments of the launch plan, such as `claude_session_id` or the account directory.
  pub backend_kwargs: Map<String, Value>,
}

/// Builds the per-process backend of one launch (`factory(option, cfg, **launch_kwargs)`).
pub trait BackendFactory: Send + Sync {
  /// `option` is `None` only for the translate-fallback type.
  fn build(
    &self,
    option: Option<&BackendOption>,
    cfg: &CharlieBotConfig,
    launch: LaunchArgs,
  ) -> anyhow::Result<Box<dyn AgentBackend>>;
}

/// How the runner reads a process's output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportKind {
  /// The process writes a raw event log.
  RawLog,
  /// The process writes its answer to stdout and exits; the runner reads the whole output after the exit.
  FinalStdout,
  /// The process serves over a local protocol (opencode).
  Serve,
}

/// What a final-stdout backend makes of its finished process (`antigravity_cli.py` `run`). The runner emits `events`,
/// records `exit_code`, and then fails the run with `guard_error` when it is set.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct FinalStdoutOutcome {
  pub events: Vec<Value>,
  pub exit_code: i32,
  pub guard_error: Option<String>,
}

/// Maps one raw event of a backend's output to the runtime's events.
pub trait Translate: Send {
  fn translate(&mut self, raw: Value) -> anyhow::Result<Vec<Value>>;
}

/// Whether the spawned child has exited, read by the mid-turn delivery path before it writes.
pub use crate::midturn_delivery::ExitedProbe;

/// The instructions file an agent backend auto-detects in its session cwd. Codex, OpenCode and Charlie Code share
/// this name; Claude Code names its own file.
pub const AGENTS_INSTRUCTIONS_FILE_NAME: &str = "AGENTS.md";

/// The per-process pieces of the backend template method. The runner in `charliebot-agents` drives them.
pub trait AgentBackend: Send {
  /// The instructions file name and the log event that reports writing it, or `None` when the backend takes none.
  fn instructions_target(&self) -> Option<(&'static str, &'static str)>;
  fn prepare_transport(&mut self, log_dir: &Path) -> anyhow::Result<()>;
  fn build_command(&self, prompt: &str) -> anyhow::Result<Vec<String>>;
  fn prepare_env(&self, env: Env) -> anyhow::Result<Env>;
  fn stdin_prompt(&self, prompt: &str) -> Option<String>;
  /// The run id whose stdin this backend keeps open for mid-turn delivery (`midturn_delivery`), or `None` for the
  /// plain-text path.
  fn midturn_run_id(&self) -> Option<String> {
    None
  }
  /// Write the run's first stdin payload and keep its stdin open for mid-turn writes (`base.py` `_write_stdin_prompt`).
  /// `exited` reports the child's exit. The runner writes `stdin_prompt` and closes stdin when this returns `None`.
  fn write_stdin_prompt(
    &self,
    _stdin: Box<dyn AsyncWrite + Send + Unpin>,
    _exited: ExitedProbe,
    _prompt: &str,
  ) -> Option<BoxFuture<'static, anyhow::Result<()>>> {
    None
  }
  /// Handle one translated event before the run's consumer sees it (`base.py` `_on_event`), or `None`.
  fn on_event<'a>(&'a self, _event: &'a Value) -> Option<BoxFuture<'a, anyhow::Result<()>>> {
    None
  }
  fn translator(&self) -> Box<dyn Translate>;
  fn transport(&self) -> TransportKind;
  /// Checks the run's uploaded files before any process starts (`base.py` `run` ignores them). `Err` ends the run
  /// with exactly one error event and no process.
  fn accept_uploads(&mut self, _uploaded_files: &[Value]) -> Result<(), String> {
    Ok(())
  }
  /// Reads the output of a `FinalStdout` process once it has exited. Only a backend whose transport is `FinalStdout`
  /// implements it; the runner calls it for no other.
  fn finish_final_stdout(&mut self, _stdout: &str, _stderr: &str, _exit_code: i32) -> FinalStdoutOutcome {
    panic!("finish_final_stdout is called only for a FinalStdout transport")
  }
  /// The CLI-native one-shot, tool-less, or `None` when the backend has none. The dispatcher
  /// `charliebot_agents::one_shot_text` runs the tool-capable one-shot for `None`.
  fn one_shot_text<'a>(
    &'a mut self,
    _base_env: &'a Env,
    _prompt: &'a str,
    _system: &'a str,
    _timeout: Option<Duration>,
  ) -> Option<BoxFuture<'a, anyhow::Result<String>>> {
    None
  }
}

/// A launch refused by the backend: the run ends with `message`. `quota_exhausted` says a spent quota caused it, and
/// `resets_at` names when that quota returns, when the backend says.
#[derive(Clone, Debug)]
pub struct LaunchRefused {
  pub message: String,
  pub quota_exhausted: bool,
  pub resets_at: Option<UtcDateTime>,
}

impl std::fmt::Display for LaunchRefused {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(&self.message)
  }
}

impl std::error::Error for LaunchRefused {}

/// Which run a launch belongs to: a master turn or a task run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchKind {
  Turn,
  Task,
}

/// Writes one event for the user. On a turn it persists and broadcasts the event; on a task it writes the run's log.
pub type EmitFn = Arc<dyn Fn(Value) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>;
/// Persists the account label on the session.
pub type RecordAccountFn = Arc<dyn Fn(String) -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>;
/// Reads the metadata of every other session with a running manager turn.
pub type RunningTurnsFn = Arc<dyn Fn() -> Vec<SessionMetadata> + Send + Sync>;
/// The session's context tokens and the time of its last request.
pub type ContextStateFn =
  Arc<dyn Fn() -> BoxFuture<'static, anyhow::Result<(Option<i64>, Option<UtcDateTime>)>> + Send + Sync>;

/// One compaction process a backend runs (`claude_compaction.CompactionRun`): its command line, the account
/// directory it reads and the chat event triggers of its outcome.
pub struct CompactionRun<'a> {
  pub argv: Vec<String>,
  pub env: Env,
  pub cwd: &'a Path,
  pub config_dir: &'a Path,
  /// The Claude session id: the transcript file name under `config_dir` and the `--resume` value.
  pub session_id: &'a str,
  pub family: &'a str,
  pub model: &'a str,
  pub trigger: &'a str,
  pub failure_trigger: Option<&'a str>,
  pub pre_tokens: Option<i64>,
  pub emit: &'a EmitFn,
  pub cfg: &'a CharlieBotConfig,
  /// The CharlieBot session id, naming the compaction process's cgroup and its per-session hooks.
  pub session_cgroup_id: Option<&'a str>,
  pub account_label: &'a str,
  pub timeout_duration: Duration,
}

/// What one compaction process did (`claude_compaction.CompactionOutcome`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactionOutcome {
  pub ok: bool,
  pub error: Option<String>,
  pub models: Vec<String>,
  pub post_tokens: Option<i64>,
  pub cache_read_tokens: Option<i64>,
}

/// Runs one compaction process (`claude_compaction.run_compaction` in the Claude Code backend).
pub type CompactionRunFn =
  Arc<dyn for<'a> Fn(CompactionRun<'a>) -> BoxFuture<'a, anyhow::Result<CompactionOutcome>> + Send + Sync>;

/// A session's context tokens and the time of its last request, as the usage owner reads them.
pub type SessionContextStateFn =
  Arc<dyn Fn(&SessionMetadata) -> anyhow::Result<(Option<i64>, Option<UtcDateTime>)> + Send + Sync>;

/// The queue's control over one session's idle-compaction start.
pub trait IdleStartGate: Send + Sync {
  /// Take the session's start reservation, only while no turn is running or queued. `true` means it was taken.
  fn reserve(&self, session_id: &str) -> anyhow::Result<bool>;
  /// Return the reservation a started compaction took.
  fn release(&self, session_id: &str) -> anyhow::Result<()>;
}

/// What the runtime gives a backend's idle-compaction hook after a round (`claude_idle_compaction.arm`).
pub struct IdleArmContext<'a> {
  /// The server handles: the config, the session store, the registry and the typed startup state.
  pub features: &'a Arc<FeatureContext>,
  pub option: &'a BackendOption,
  pub session_id: &'a str,
  /// The session's context tokens and last request time. The trigger path reads it again on each inspection.
  pub context_state: SessionContextStateFn,
  /// The session directory the compaction process runs in.
  pub cwd: PathBuf,
  /// The base agent environment of the compaction process.
  pub base_env: Env,
  /// Writes one event for the user.
  pub emit: EmitFn,
  /// The queue's compaction runner; `None` runs the hook's own.
  pub compaction: Option<CompactionRunFn>,
  /// The queue's start reservation.
  pub gate: Arc<dyn IdleStartGate>,
}

/// What the runtime gives a lifecycle for one run.
pub struct LaunchContext {
  pub cfg: Arc<CharlieBotConfig>,
  pub option: BackendOption,
  /// Shared turn metadata, updated when the run records a new Claude account label.
  pub session_meta: Arc<Mutex<SessionMetadata>>,
  pub kind: LaunchKind,
  pub run_kind: RunKind,
  pub cwd: PathBuf,
  /// The base agent environment owned by the startup state.
  pub base_env: Arc<Env>,
  /// The conversation this run may resume. `None` starts fresh.
  pub held_native_id: Option<String>,
  /// For a task run, the conversation id the runtime chose for its first process.
  pub preassigned_native_id: Option<String>,
  pub emit: EmitFn,
  pub record_account: RecordAccountFn,
  pub context_state: ContextStateFn,
  /// Present for manager turns; task launches carry `None`.
  pub running_turns: Option<RunningTurnsFn>,
  /// The task-tree Run id, when the turn belongs to a Run (`backend_lifecycle.LaunchContext.run_id`).
  pub run_id: Option<String>,
}

/// The plan for one process of a run: the factory arguments, the conversation to resume, and after a relay the
/// prompt to send.
pub struct Launch {
  pub backend_kwargs: Map<String, Value>,
  pub resume_id: Option<String>,
  /// Replaces the run's prompt when set. A relay sets the continuation prompt.
  pub prompt: Option<String>,
  /// The login that the run's log lines name.
  pub account_label: Option<String>,
  /// Lifecycle-private state, such as a relay count. The runtime does not read it.
  pub private: Option<Arc<dyn Any + Send + Sync>>,
}

/// Folds one process's events and exit into the decision to start a next process.
pub trait LaunchWatch: Send {
  /// Records one event. `true` stops the process now, at a safe relay point.
  fn observe(&mut self, event: &Value) -> bool;
  /// `true`: the run continues in a next process, which `next_launch` plans.
  fn wants_next(&self, exit_code: i32, stderr: &str) -> bool;
}

/// A backend type's lifecycle (`backend_lifecycle.BackendLifecycle`). The defaults serve a type with no login pool:
/// one process per run, its conversation resumed as held.
pub trait BackendLifecycle: Send + Sync {
  /// The domain in which backend options continue each other's conversation. One option is its own domain.
  fn continuation_domain(&self, option: &BackendOption, _cfg: &CharlieBotConfig) -> String {
    option.id.clone()
  }

  /// Plans the first process of the run, or refuses it.
  fn place<'a>(&'a self, ctx: &'a LaunchContext) -> BoxFuture<'a, Result<Launch, LaunchRefused>> {
    Box::pin(async move {
      Ok(Launch {
        backend_kwargs: Map::new(),
        resume_id: ctx.held_native_id.clone(),
        prompt: None,
        account_label: None,
        private: None,
      })
    })
  }

  /// The watch of the process that `launch` plans, or `None` for no relay.
  fn watch(&self, _ctx: &LaunchContext, _launch: &Launch) -> Option<Box<dyn LaunchWatch>> {
    None
  }

  /// Plans the next process after `launch`, or refuses the relay. Only a lifecycle whose watch asks for relays
  /// implements it; the default is never called, because its watch is `None`.
  fn next_launch<'a>(
    &'a self,
    _ctx: &'a LaunchContext,
    _launch: &'a Launch,
    _native_id: Option<&'a str>,
  ) -> BoxFuture<'a, Result<Launch, LaunchRefused>> {
    Box::pin(async { panic!("next_launch is called only for a lifecycle whose watch asks for a relay") })
  }

  /// Cleans up after the last process of a round, whether the round succeeded or not.
  fn after_round<'a>(
    &'a self,
    _ctx: &'a LaunchContext,
    _native_id: Option<&'a str>,
    _succeeded: bool,
  ) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async { Ok(()) })
  }

  /// The launch arguments that turn on this backend's mid-turn input path for the turn, or an empty map when the
  /// backend keeps stdin closed (`midturn_delivery.launch_kwargs`).
  fn midturn_launch_kwargs(
    &self,
    _meta: &SessionMetadata,
    _turn: bool,
    _run_kind: RunKind,
    _run_id: Option<&str>,
  ) -> anyhow::Result<Map<String, Value>> {
    Ok(Map::new())
  }

  /// Arms the session's idle compaction after a round (`claude_idle_compaction.arm`). The default arms none.
  fn arm_idle_compaction<'a>(&'a self, _ctx: IdleArmContext<'a>) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async { Ok(()) })
  }

  /// Drops the session's pending idle timer, when this backend keeps one.
  fn disarm_idle_compaction(&self, _session_id: &str) {}

  /// Waits out the session's running idle compaction before a turn starts
  /// (`claude_idle_compaction.settle_for_turn`). The default has none to wait for.
  fn settle_idle_compaction<'a>(&'a self, _session_id: &'a str) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async { Ok(()) })
  }

  /// The notice events for the user that a finished round's events call for.
  fn round_notices(&self, _option: &BackendOption, _events: &[Value]) -> Vec<Value> {
    Vec::new()
  }

  /// True when `round_notices` can return notices (`launch_loop.py`).
  fn has_round_notices(&self) -> bool {
    false
  }

  /// The name that logs and error events give the account label; empty when the backend keeps none.
  fn account_source(&self) -> &'static str {
    ""
  }

  /// The login whose transcript store holds the session's conversation; `None` when unrecorded.
  fn account_label(&self, _meta: &SessionMetadata) -> Option<String> {
    None
  }

  /// Records `label` on `meta` in memory. Returns `true` when the recorded label changed.
  fn record_account_label(&self, _meta: &mut SessionMetadata, _label: Option<&str>) -> bool {
    false
  }

  /// The conversation id the runtime chose for the task before its first process started.
  fn thread_native_id(&self, _thread: &ThreadMetadata) -> Option<String> {
    None
  }

  /// Records on `thread` the conversation id the runtime chose for the task.
  fn assign_thread_native_id(&self, _thread: &mut ThreadMetadata, _native_id: Option<&str>) {}

  /// The serve transport of this type's processes, when they answer each turn over a local protocol
  /// (`TransportKind::Serve`). The default is `None`: the processes write a raw log or their final stdout.
  fn serve_transport(&self) -> Option<Arc<dyn ServeTransport>> {
    None
  }
}

/// How the runner runs a turn of a serve backend type. The runner takes the turn's run plan before it spawns the
/// process, then runs the turn in that process.
pub trait ServeTransport: Send + Sync {
  /// Takes the run plan that the backend registered for the process `command` starts. An error ends the run before
  /// any process starts.
  fn take_turn(&self, command: &[String]) -> anyhow::Result<Box<dyn ServeTurn>>;
  /// Finishes the turn whose process is `pid`, when that turn has registered its session.
  fn finish_active_turn(&self, pid: u32) -> BoxFuture<'static, anyhow::Result<()>>;
  /// The progress deadline of a recovered turn whose run directory is `directory`, or `None` when the directory
  /// holds no turn record of this backend. The caller builds the follow's silence callback from it.
  fn recovery_silence(&self, directory: &Path) -> anyhow::Result<Option<RecoverySilence>>;
  /// Rejects a raw event this backend cannot answer headless, ending the run. Called for every followed event of a
  /// recovered turn.
  fn reject_event(&self, event: &Value) -> anyhow::Result<()>;
}

/// A recovered serve turn's progress deadline: a follow that sees no event for `timeout` fails with `message`.
pub struct RecoverySilence {
  pub timeout: Duration,
  pub message: String,
}

/// One turn of a serve transport, run in the process that the runner spawned for it.
pub trait ServeTurn: Send {
  /// The detached identity process that owns this turn. `None` uses the backend command as the identity process.
  fn identity_process(&self, _command: &[String], _run_directory: &Path) -> anyhow::Result<Option<ServeProcessSpec>> {
    Ok(None)
  }

  /// Runs the turn to its end in the process that `args` describes.
  fn run(self: Box<Self>, args: ServeTurnArgs) -> BoxFuture<'static, anyhow::Result<ServeTurnResult>>;
}

/// A process command chosen by a serve backend to carry one run's identity.
pub struct ServeProcessSpec {
  pub program: OsString,
  pub args: Vec<OsString>,
  pub env: Env,
}

/// What the runner gives a serve turn once its process has spawned.
pub struct ServeTurnArgs {
  pub prompt: String,
  pub cwd: PathBuf,
  pub run_directory: PathBuf,
  pub raw_log: PathBuf,
  pub raw_cursor: PathBuf,
  pub buffer_limit: usize,
  pub post_result_timeout: Duration,
  pub env: Env,
  pub uploaded_files: Vec<Value>,
  /// The turn process's pid. `ServeTransport::finish_active_turn` finds the turn by it.
  pub process_id: u32,
  /// The directory of the session cgroup that the process runs in, when it runs in one.
  pub cgroup_path: Option<PathBuf>,
  /// `None` while the turn process runs; then its exit code, or the error that ended it.
  pub exit: watch::Receiver<Option<Result<i32, String>>>,
  pub on_event: EmitFn,
}

/// The exit code that a serve turn chose, when it chose one.
pub struct ServeTurnResult {
  pub exit_code: Option<i32>,
}

impl Registry {
  /// Register a child-process environment edit (`backend_lifecycle_registration.register_child_env`). The edits apply
  /// in registration order to every agent child process.
  pub fn child_env(&mut self, name: &'static str, edit: ChildEnvFn) -> Result<(), RegistryError> {
    let package = self.owner()?;
    self.child_envs.insert(package, name, edit)
  }

  /// Register the context limits of a reading kind (`register_reading_limits`).
  pub fn reading_limits(&mut self, kind: &'static str, limits: ReadingLimitsFn) -> Result<(), RegistryError> {
    let package = self.owner()?;
    self.reading_limits.insert(package, kind, limits)
  }

  /// Register context limits selected by backend type and reading kind.
  pub fn backend_reading_limits(
    &mut self,
    backend_type: &'static str,
    kind: &'static str,
    limits: BackendReadingLimitsFn,
  ) -> Result<(), RegistryError> {
    let package = self.owner()?;
    if let Some(existing) = self
      .backend_reading_limits
      .iter()
      .find(|registration| registration.backend_type == backend_type && registration.reading_kind == kind)
    {
      return Err(RegistryError {
        package,
        message: format!(
          "backend reading limits for ({backend_type:?}, {kind:?}) are already registered by {}",
          existing.package
        ),
      });
    }
    self.backend_reading_limits.push(BackendReadingLimitsRegistration {
      backend_type,
      reading_kind: kind,
      package,
      limits,
    });
    Ok(())
  }

  /// Register the usage resolver of a backend type (`register_usage_resolver`).
  pub fn usage_resolver(
    &mut self,
    backend_type: &'static str,
    constructor: UsageResolverConstructor,
  ) -> Result<(), RegistryError> {
    let package = self.owner()?;
    self.usage_resolvers.insert(package, backend_type, constructor)
  }

  /// The registration of `backend_type`, or `None` when no package registered it.
  pub fn backend_type_of(&self, backend_type: &str) -> Option<&BackendTypeRegistration> {
    self.backend_types.get(backend_type)
  }

  /// The lifecycle of `backend_type`: its registered lifecycle, or the default one. An unregistered type fails.
  pub fn lifecycle_for(&self, backend_type: &str) -> anyhow::Result<Arc<dyn BackendLifecycle>> {
    match self.backend_types.get(backend_type) {
      Some(registration) => Ok(registration.lifecycle.clone().unwrap_or_else(|| Arc::clone(&self.default_lifecycle))),
      None => bail!("Unknown backend type: {backend_type}"),
    }
  }

  /// The serve transport of `backend_type`, or `None` when the type registered none.
  pub fn serve_transport(&self, backend_type: &str) -> Option<Arc<dyn ServeTransport>> {
    self.backend_types.get(backend_type)?.lifecycle.as_ref()?.serve_transport()
  }

  /// Every registered lifecycle, each once, in registration order. Types without one are left out.
  pub fn lifecycles(&self) -> Vec<Arc<dyn BackendLifecycle>> {
    let mut found: Vec<Arc<dyn BackendLifecycle>> = Vec::new();
    for registration in self.backend_types.values() {
      if let Some(lifecycle) = &registration.lifecycle
        && !found.iter().any(|known| Arc::ptr_eq(known, lifecycle))
      {
        found.push(Arc::clone(lifecycle));
      }
    }
    found
  }

  /// Drops the session's pending idle timer on every backend that keeps one: a session that changes backend keeps
  /// none.
  pub fn disarm_idle_compaction(&self, session_id: &str) {
    for lifecycle in self.lifecycles() {
      lifecycle.disarm_idle_compaction(session_id);
    }
  }

  /// Waits out the session's running idle compaction on every backend that keeps one.
  pub async fn settle_idle_compaction(&self, session_id: &str) -> anyhow::Result<()> {
    for lifecycle in self.lifecycles() {
      lifecycle.settle_idle_compaction(session_id).await?;
    }
    Ok(())
  }

  /// The type whose family prefix starts `backend_id`, or `None`. It names the type of a backend id that config no
  /// longer defines.
  pub fn type_for_family_prefix(&self, backend_id: &str) -> Option<&'static str> {
    self.backend_types.values().find_map(|registration| {
      let prefix = registration.traits.family_prefix?;
      backend_id.starts_with(prefix).then_some(registration.type_name)
    })
  }

  /// The one type that serves the translate fallback. None registered is an error.
  pub fn translate_fallback(&self) -> anyhow::Result<&BackendTypeRegistration> {
    match self.backend_types.values().find(|registration| registration.translate_fallback) {
      Some(registration) => Ok(registration),
      None => bail!("no backend type serves the translate fallback"),
    }
  }

  /// Builds the backend of `option`'s type with its factory.
  pub fn build_backend(
    &self,
    option: &BackendOption,
    cfg: &CharlieBotConfig,
    launch: LaunchArgs,
  ) -> anyhow::Result<Box<dyn AgentBackend>> {
    let Some(registration) = self.backend_types.get(&option.backend_type) else {
      bail!("Unknown backend type: {}", option.backend_type);
    };
    registration.factory.build(Some(option), cfg, launch)
  }

  /// Applies every child-environment edit to `env`, in registration order.
  pub fn apply_child_env(&self, env: &mut Env) {
    for edit in self.child_envs.values() {
      edit(env);
    }
  }

  /// The context limits of `kind`, or `None` when no package registered the kind.
  /// A matching backend provider takes precedence when `option` names its type.
  pub fn reading_limits_for(
    &self,
    kind: &str,
    run_kind: RunKind,
    option: Option<&BackendOption>,
  ) -> Option<ContextLimits> {
    if let Some(option) = option
      && let Some(registration) = self
        .backend_reading_limits
        .iter()
        .find(|registration| registration.backend_type == option.backend_type && registration.reading_kind == kind)
    {
      return Some((registration.limits)(option, run_kind));
    }
    self.reading_limits.get(kind).map(|limits| limits(run_kind))
  }

  /// The usage-resolver constructor of `backend_type`, or `None` when the type has none.
  pub fn usage_resolver_for(&self, backend_type: &str) -> Option<UsageResolverConstructor> {
    self.usage_resolvers.get(backend_type).cloned()
  }
}

/// Builds a package's routes with their full paths, with the feature context applied as the router's state. The
/// server merges the routers in registration order.
pub type RouterBuilder = fn(&Arc<FeatureContext>) -> Router;

/// Whether a request path falls under one of a router's routes, whatever the method. The trailing-slash redirect
/// asks it of every router, as it asks the server's own route families.
pub type PathMatch = fn(&str) -> bool;

/// The request data a registered WebSocket route receives after the gate authorizes its upgrade.
pub struct WebSocketRequest {
  /// The decoded request path.
  pub path: String,
  /// The decoded path segments captured by `*`, in pattern order.
  pub segments: Vec<String>,
  /// The raw query string, or an empty string when the request has no query.
  pub query: String,
  /// The upgrade that the route accepts to start its WebSocket connection.
  pub upgrade: WebSocketUpgrade,
}

/// One WebSocket handler. The gate supplies the same feature context used to build package routers.
pub type WebSocketHandler = fn(Arc<FeatureContext>, WebSocketRequest) -> BoxFuture<'static, Response>;

/// A matching WebSocket registration and the path segments captured by its `*` wildcards.
pub struct WebSocketMatch {
  /// The registered handler.
  pub handler: WebSocketHandler,
  /// The decoded segments captured by `*`, in pattern order.
  pub segments: Vec<String>,
}

/// Which request paths a router claims, whatever the method. The trailing-slash redirect asks every router.
pub enum PathClaim {
  /// The route patterns the router mounts.
  Patterns(RouteClaims),
  /// A predicate over the decoded path, for a router whose claims are not route patterns.
  Predicate(PathMatch),
}

/// One registered router.
pub struct RouterRegistration {
  pub package: &'static str,
  pub build: RouterBuilder,
  pub claim: PathClaim,
}

impl RouterRegistration {
  /// Whether the router claims a decoded request path.
  pub fn claims(&self, path: &str) -> bool {
    match &self.claim {
      PathClaim::Patterns(routes) => routes.claims(path),
      PathClaim::Predicate(matches) => matches(path),
    }
  }
}

/// When a service starts: `Early` right after the crash-recovery step starts, `Ready` after the trigger step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
  Early,
  Ready,
}

/// What the lifespan hands each service's `start`.
pub struct ServiceContext {
  pub features: Arc<FeatureContext>,
  /// Resolves when the crash-recovery step ends. A service that reads recovered state awaits it.
  pub recovery_done: Shared<BoxFuture<'static, ()>>,
}

/// A background service (`wiring.register_service`). A service stops in reverse registration order, ready
/// services first. `stop` returns at once when `start` never ran.
pub trait Service: Send + Sync {
  fn name(&self) -> &'static str;
  fn phase(&self) -> Phase;
  fn start<'a>(&'a self, ctx: ServiceContext) -> BoxFuture<'a, anyhow::Result<()>>;
  fn stop<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<()>>;
}

/// A file view (`wiring.register_file_view`). The file server asks each view, in registration order, before it
/// serves a path; `path` is the absolute path without its leading "/". `Ok(Some)` answers the request, `Ok(None)`
/// passes it on, and `Err` is the error response (Python raises `HTTPException`).
pub trait FileView: Send + Sync {
  fn view<'a>(
    &'a self,
    features: &'a FeatureContext,
    request: &'a Parts,
    path: &'a str,
  ) -> BoxFuture<'a, Result<Option<Response>, Response>>;
}

/// A config check (`config_registry.register_config_check`) runs after every config load. It fails with its message.
pub type CheckFn = fn(&CharlieBotConfig) -> Result<(), String>;

/// A startup check (`wiring.register_startup_check`) runs once before the server serves. It reads the registry, so it
/// can resolve the usage sources and backend attributions the packages registered. It fails with its message.
pub type StartupCheckFn = fn(&Registry, &CharlieBotConfig) -> Result<(), String>;

/// A diff root (`wiring.register_diff_root`): the diff view also accepts repositories under the path it returns.
pub type DiffRootFn = fn(&CharlieBotConfig) -> PathBuf;

/// A scheduled handler (`scheduled_handlers.register_handler`): a cron task with `handler: <name>` awaits it, and
/// the `Ok` string is the fire's summary.
pub type ScheduledHandlerFn = fn(Arc<FeatureContext>) -> BoxFuture<'static, anyhow::Result<String>>;

impl Registry {
  /// Register a router (`wiring.register_router`). Routers merge in registration order.
  pub fn router(&mut self, build: RouterBuilder, matches: PathMatch) -> Result<(), RegistryError> {
    let package = self.owner()?;
    self.routers.push(RouterRegistration { package, build, claim: PathClaim::Predicate(matches) });
    Ok(())
  }

  /// Register a router by the route patterns it mounts, in axum's syntax (`wiring.register_router`). Routers merge
  /// in registration order.
  pub fn router_patterns(&mut self, build: RouterBuilder, patterns: &[&str]) -> Result<(), RegistryError> {
    let package = self.owner()?;
    let claims = RouteClaims::new(patterns).map_err(|error| RegistryError { package, message: error.to_string() })?;
    self.routers.push(RouterRegistration { package, build, claim: PathClaim::Patterns(claims) });
    Ok(())
  }

  /// Register an authenticated WebSocket route. Each `*` matches one non-empty path segment.
  pub fn websocket(&mut self, pattern: &'static str, handler: WebSocketHandler) -> Result<(), RegistryError> {
    let package = self.owner()?;
    self.websocket_routes.insert(package, pattern, handler)
  }

  /// The first registered handler whose pattern matches `path`, with captured segments in pattern order.
  pub fn websocket_match(&self, path: &str) -> Option<WebSocketMatch> {
    let path_segments: Vec<&str> = path.split('/').collect();
    self.websocket_routes.pairs().find_map(|(pattern, handler)| {
      let pattern_segments: Vec<&str> = pattern.split('/').collect();
      if pattern_segments.len() != path_segments.len() {
        return None;
      }
      let mut segments = Vec::new();
      for (expected, actual) in pattern_segments.iter().zip(&path_segments) {
        if *expected == "*" {
          if actual.is_empty() {
            return None;
          }
          segments.push((*actual).to_string());
        } else if expected != actual {
          return None;
        }
      }
      Some(WebSocketMatch { handler: *handler, segments })
    })
  }

  /// Register a service under its own name (`wiring.register_service`).
  pub fn service(&mut self, service: Arc<dyn Service>) -> Result<(), RegistryError> {
    let package = self.owner()?;
    let name = service.name();
    self.services.insert(package, name, service)
  }

  /// Register the plan-registry reader. `web_reply` reads a session's plan links through it.
  pub fn plan_registry(&mut self, read: PlanRegistryFn) -> Result<(), RegistryError> {
    let package = self.owner()?;
    self.plan_registry.insert_single(package, "plan_registry", read)
  }

  /// The registered plan-registry reader; a server with none fails rather than reading plan links without it.
  pub fn plan_registry_reader(&self) -> anyhow::Result<PlanRegistryFn> {
    self.plan_registry.values().next().copied().ok_or_else(|| anyhow::anyhow!("no package registers the plan registry"))
  }

  /// Register the reader of every open session's Waiting on you items. `approval` reads the day view through it.
  pub fn open_waiting_items(&mut self, read: OpenWaitingItemsFn) -> Result<(), RegistryError> {
    let package = self.owner()?;
    self.open_waiting_items.insert_single(package, "open_waiting_items", read)
  }

  /// The registered open Waiting on you reader; a server with none fails rather than listing no user-act items.
  pub fn open_waiting_items_reader(&self) -> anyhow::Result<OpenWaitingItemsFn> {
    self
      .open_waiting_items
      .values()
      .next()
      .copied()
      .ok_or_else(|| anyhow::anyhow!("no package registers the open Waiting on you reader"))
  }

  /// Register the fork-assertion runner. `web_reply`'s status gate runs a linked page through it.
  pub fn fork_assertions(&mut self, run: ForkAssertionsFn) -> Result<(), RegistryError> {
    let package = self.owner()?;
    self.fork_assertions.insert_single(package, "fork_assertions", run)
  }

  /// The registered fork-assertion runner; a host with none fails rather than gating a page without it.
  pub fn fork_assertions_runner(&self) -> anyhow::Result<ForkAssertionsFn> {
    self
      .fork_assertions
      .values()
      .next()
      .copied()
      .ok_or_else(|| anyhow::anyhow!("no package registers the fork assertions"))
  }

  /// Register a file view (`wiring.register_file_view`).
  pub fn file_view(&mut self, name: &'static str, view: Arc<dyn FileView>) -> Result<(), RegistryError> {
    let package = self.owner()?;
    self.file_views.insert(package, name, view)
  }

  /// Register a startup check (`wiring.register_startup_check`).
  pub fn startup_check(&mut self, name: &'static str, check: StartupCheckFn) -> Result<(), RegistryError> {
    let package = self.owner()?;
    self.startup_checks.insert(package, name, check)
  }

  /// Register a config check (`config_registry.register_config_check`).
  pub fn config_check(&mut self, name: &'static str, check: CheckFn) -> Result<(), RegistryError> {
    let package = self.owner()?;
    self.config_checks.insert(package, name, check)
  }

  /// Register a diff root (`wiring.register_diff_root`).
  pub fn diff_root(&mut self, name: &'static str, root: DiffRootFn) -> Result<(), RegistryError> {
    let package = self.owner()?;
    self.diff_roots.insert(package, name, root)
  }

  /// Register a scheduled handler under its task name (`scheduled_handlers.register_handler`).
  pub fn scheduled_handler(&mut self, name: &'static str, handler: ScheduledHandlerFn) -> Result<(), RegistryError> {
    let package = self.owner()?;
    self.scheduled_handlers.insert(package, name, handler)
  }

  /// The routers in registration order.
  pub fn routers(&self) -> &[RouterRegistration] {
    &self.routers
  }

  /// The services that start in `phase`, in registration order.
  pub fn services_in(&self, phase: Phase) -> Vec<Arc<dyn Service>> {
    self.services.values().filter(|service| service.phase() == phase).cloned().collect()
  }

  /// The file views in registration order.
  pub fn file_views(&self) -> impl Iterator<Item = (&'static str, &Arc<dyn FileView>)> {
    self.file_views.pairs()
  }

  /// The startup checks in registration order.
  pub fn startup_checks(&self) -> impl Iterator<Item = (&'static str, StartupCheckFn)> + '_ {
    self.startup_checks.pairs().map(|(name, check)| (name, *check))
  }

  /// The diff roots in registration order.
  pub fn diff_roots(&self) -> impl Iterator<Item = (&'static str, DiffRootFn)> + '_ {
    self.diff_roots.pairs().map(|(name, root)| (name, *root))
  }

  /// The scheduled handler registered under `name`, or `None` when no package registered it.
  pub fn scheduled_handler_named(&self, name: &str) -> Option<ScheduledHandlerFn> {
    self.scheduled_handlers.get(name).copied()
  }
}

/// A zero-argument callable a template calls at render (`page_render.register_template_global`). The function sees
/// the live config and credentials through the feature context. An `Err` fails the render with its message.
pub type TemplateGlobalFn = fn(&FeatureContext) -> Result<minijinja::Value, String>;

/// One `/home` page card (`page_render.register_home_card`). Cards list in registration order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HomeCard {
  pub name: &'static str,
  pub url: &'static str,
  pub description: &'static str,
}

impl Registry {
  /// Register a template global (`page_render.register_template_global`).
  pub fn template_global(&mut self, name: &'static str, global: TemplateGlobalFn) -> Result<(), RegistryError> {
    let package = self.owner()?;
    self.template_globals.insert(package, name, global)
  }

  /// Register a home-page card (`page_render.register_home_card`).
  pub fn home_card(&mut self, card: HomeCard) -> Result<(), RegistryError> {
    self.owner()?;
    self.home_cards.push(card);
    Ok(())
  }

  /// The registered packages in registration order: the template search path and the static mounts follow it.
  pub fn page_packages(&self) -> &[&'static str] {
    &self.page_packages
  }

  /// The template globals with their names, in registration order.
  pub fn template_globals(&self) -> impl Iterator<Item = (&'static str, TemplateGlobalFn)> + '_ {
    self.template_globals.pairs().map(|(name, global)| (name, *global))
  }

  /// The home-page cards in registration order.
  pub fn home_cards(&self) -> &[HomeCard] {
    &self.home_cards
  }
}

/// The workflow rules file a session gets when no contribution names one (`DEFAULT_WORKFLOW_RULES_FILE`).
pub const DEFAULT_WORKFLOW_RULES_FILE: &str = "manager_workflows.md";

/// Where an instruction segment's text came from, as the runtime's prompt assembly records it (`PromptSource`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptSource {
  pub scope: String,
  pub source_ref: String,
  pub source_session_id: Option<String>,
}

/// One ordered instruction unit (`task_prompts.RuleSegment`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleSegment {
  pub text: String,
  pub sources: Vec<PromptSource>,
  pub delivery: String,
}

/// Renders one persisted event type for the chat: the event in, the message out.
pub type EventRenderer = fn(&Value) -> Value;

/// One package's part in a master turn. Every step defaults to adding nothing.
pub trait TurnContribution: Send + Sync {
  /// The Python class this contribution ports, named as `type(contribution).__name__` names it. The store names the
  /// contribution's after-turn task by it: `after-turn-<class name>-<session id>`.
  fn class_name(&self) -> &'static str;

  /// Segments for the managed instructions of a run of `kind`. They follow the model overlay segments and precede
  /// the local rule segments, in registration order. A contribution reads its prompt docs through `repo`.
  fn instruction_segments(
    &self,
    _meta: &SessionMetadata,
    _kind: &str,
    _cfg: &CharlieBotConfig,
    _repo: &RepoRoot,
  ) -> anyhow::Result<Vec<RuleSegment>> {
    Ok(Vec::new())
  }

  /// The workflow rules file for this session, or `None` to name none. At most one contribution answers `Some`.
  fn workflow_rules_file(&self, _meta: &SessionMetadata) -> Option<&'static str> {
    None
  }

  /// Runs when a turn's input is queued, before the user event is appended.
  fn before_turn<'a>(
    &'a self,
    _meta: &'a SessionMetadata,
    _cfg: &'a CharlieBotConfig,
    _features: &'a Arc<FeatureContext>,
  ) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async { Ok(()) })
  }

  /// Runs as its own logged task after the `MASTER_DONE` event is appended and broadcast.
  fn after_turn<'a>(
    &'a self,
    _meta: &'a SessionMetadata,
    _done_event: &'a Value,
    _cfg: &'a CharlieBotConfig,
    _features: &'a Arc<FeatureContext>,
  ) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async { Ok(()) })
  }

  /// The line that opens a user input in the turn prompt. Several contributions' lines join in registration order.
  fn input_preamble(&self, _meta: &SessionMetadata, _input_event: &Value) -> Option<String> {
    None
  }

  /// Chat renderers by event type. A type that two contributions render fails the registration.
  fn event_renderers(&self) -> Vec<(&'static str, EventRenderer)> {
    Vec::new()
  }

  /// True when this contribution marks the session unread itself after a clean turn, so the runtime skips its mark.
  fn claims_turn_end_unread(&self, _meta: &SessionMetadata) -> bool {
    false
  }
}

impl Registry {
  /// Register a turn contribution (`turn_contributions.register_turn_contribution`). A chat renderer type that an
  /// earlier contribution, or this one, already renders fails here: Python fails at the aggregator's build.
  pub fn turn_contribution(
    &mut self,
    name: &'static str,
    contribution: Arc<dyn TurnContribution>,
  ) -> Result<(), RegistryError> {
    let package = self.owner()?;
    let mut rendered: Vec<&'static str> = self
      .turn_contributions
      .values()
      .flat_map(|known| known.event_renderers().into_iter().map(|(event_type, _)| event_type))
      .collect();
    for (event_type, _) in contribution.event_renderers() {
      if rendered.contains(&event_type) {
        return Err(RegistryError {
          package,
          message: format!("event type {event_type:?} is rendered by more than one turn contribution"),
        });
      }
      rendered.push(event_type);
    }
    self.turn_contributions.insert(package, name, contribution)
  }

  /// The workflow rules file for `meta`: the one a contribution names, else [`DEFAULT_WORKFLOW_RULES_FILE`].
  /// Two contributions that name a file fail.
  pub fn workflow_rules_file(&self, meta: &SessionMetadata) -> anyhow::Result<&'static str> {
    let answers = self.turn_contributions.values().filter_map(|contribution| contribution.workflow_rules_file(meta));
    let named: Vec<&'static str> = answers.collect();
    match named.as_slice() {
      [] => Ok(DEFAULT_WORKFLOW_RULES_FILE),
      [file] => Ok(file),
      _ => bail!("workflow rules file: more than one turn contribution answers: {}", named.join(", ")),
    }
  }

  /// The instruction segments of every contribution, in registration order.
  pub fn instruction_segments(
    &self,
    meta: &SessionMetadata,
    kind: &str,
    cfg: &CharlieBotConfig,
    repo: &RepoRoot,
  ) -> anyhow::Result<Vec<RuleSegment>> {
    let mut segments = Vec::new();
    for contribution in self.turn_contributions.values() {
      segments.extend(contribution.instruction_segments(meta, kind, cfg, repo)?);
    }
    Ok(segments)
  }

  /// The input preamble of `input_event`: the preambles that the contributions give, one line each, in registration
  /// order; `None` when no contribution gives one.
  pub fn input_preamble(&self, meta: &SessionMetadata, input_event: &Value) -> Option<String> {
    let preambles: Vec<String> = self
      .turn_contributions
      .values()
      .filter_map(|contribution| contribution.input_preamble(meta, input_event))
      .collect();
    (!preambles.is_empty()).then(|| preambles.join("\n"))
  }

  /// True when a registered contribution marks the turn-end unread flag for `meta` itself.
  pub fn turn_end_unread_claimed(&self, meta: &SessionMetadata) -> bool {
    self.turn_contributions.values().any(|contribution| contribution.claims_turn_end_unread(meta))
  }

  /// Run every contribution's `before_turn` in registration order.
  pub async fn before_turn(
    &self,
    meta: &SessionMetadata,
    cfg: &CharlieBotConfig,
    features: &Arc<FeatureContext>,
  ) -> anyhow::Result<()> {
    for contribution in self.turn_contributions.values() {
      contribution.before_turn(meta, cfg, features).await?;
    }
    Ok(())
  }
}

impl Registry {
  /// Register a sequence controller (`sequence_controllers.register_sequence_controller`). A repeated name fails.
  pub fn sequence_controller(
    &mut self,
    name: &'static str,
    controller: Arc<dyn SequenceController>,
  ) -> Result<(), RegistryError> {
    let package = self.owner()?;
    self.sequence_controllers.insert(package, name, controller)
  }

  /// The registered controllers in registration order.
  pub fn sequence_controllers(&self) -> impl Iterator<Item = &Arc<dyn SequenceController>> {
    self.sequence_controllers.values()
  }

  /// Whether a registered controller owns `meta`.
  pub fn sequence_owns_session(&self, meta: &SessionMetadata) -> bool {
    self.sequence_controllers().any(|controller| controller.owns_session(meta))
  }

  /// The binding of `session_id`. Two controllers that both bind one session fail.
  pub fn binding_for(&self, session_id: &str) -> anyhow::Result<Option<Arc<dyn SequenceBinding>>> {
    let mut found: Option<Arc<dyn SequenceBinding>> = None;
    for controller in self.sequence_controllers() {
      let Some(binding) = controller.binding(session_id) else {
        continue;
      };
      if found.is_some() {
        anyhow::bail!("multiple sequence controllers bind session {session_id:?}");
      }
      found = Some(binding);
    }
    Ok(found)
  }

  /// Merges every controller's listing fields into one row map. Every requested id has an entry, possibly empty.
  pub fn sequence_listing_fields(
    &self,
    session_ids: &[String],
    now: UtcDateTime,
  ) -> HashMap<String, Map<String, Value>> {
    let mut ids = session_ids.to_vec();
    ids.sort();
    ids.dedup();
    let mut fields: HashMap<String, Map<String, Value>> = ids.iter().map(|id| (id.clone(), Map::new())).collect();
    for controller in self.sequence_controllers() {
      for (session_id, controller_fields) in controller.listing_fields(&ids, now) {
        fields.entry(session_id).or_default().extend(controller_fields);
      }
    }
    fields
  }
}

impl Registry {
  /// Register a sidebar contribution (`sidebar_contributions.register_sidebar_contribution`).
  pub fn sidebar_contribution(
    &mut self,
    name: &'static str,
    contribution: Arc<dyn SidebarContribution>,
  ) -> Result<(), RegistryError> {
    let package = self.owner()?;
    self.sidebar_contributions.insert(package, name, contribution)
  }

  /// The sidebar contributions in registration order.
  pub fn sidebar_contributions(&self) -> impl Iterator<Item = &Arc<dyn SidebarContribution>> {
    self.sidebar_contributions.values()
  }
}

impl Registry {
  /// Register a usage source (`usage_source_registration.register_source`). A repeated name, or an id prefix that an
  /// earlier source holds, fails.
  pub fn usage_source(&mut self, source: UsageSource) -> Result<(), RegistryError> {
    let package = self.owner()?;
    for prefix in source.id_prefixes {
      if let Some(owner) = self.usage_sources.values().find(|known| known.id_prefixes.contains(prefix)) {
        return Err(RegistryError {
          package,
          message: format!("usage source {:?}: id prefix {prefix:?} already belongs to {:?}", source.name, owner.name),
        });
      }
    }
    self.usage_sources.insert(package, source.name, source)
  }

  /// Attribute the usage of backends of `backend_type` to the source named `source`
  /// (`usage_source_registration.attribute_backend_type`). The source may register after the attribution.
  pub fn attribute_backend_type(
    &mut self,
    backend_type: &'static str,
    source: &'static str,
  ) -> Result<(), RegistryError> {
    let package = self.owner()?;
    self.usage_attributions.insert(package, backend_type, source)
  }

  /// The usage sources in registration order.
  pub fn usage_sources(&self) -> impl Iterator<Item = &UsageSource> {
    self.usage_sources.values()
  }

  /// The source `backend_type` is attributed to, or `None` for a type with no usage source. An attribution to an
  /// unregistered source fails.
  pub fn usage_source_for(&self, backend_type: &str) -> anyhow::Result<Option<&UsageSource>> {
    let Some(name) = self.usage_attributions.get(backend_type) else {
      return Ok(None);
    };
    match self.usage_sources.get(name) {
      Some(source) => Ok(Some(source)),
      None => bail!("backend type {backend_type:?} is attributed to usage source {name:?}, which is not registered"),
    }
  }

  /// The quota accounts of every source that declares `quota_accounts`, in source registration order. The first source
  /// error stops the call.
  pub fn quota_accounts(&self, cfg: &CharlieBotConfig) -> anyhow::Result<Vec<Arc<dyn QuotaAccount>>> {
    let mut accounts = Vec::new();
    for source in self.usage_sources.values() {
      let Some(implementation) = source.implementation.as_ref() else {
        continue;
      };
      if implementation.ops().quota_accounts {
        accounts.extend(implementation.quota_accounts(cfg)?);
      }
    }
    Ok(accounts)
  }

  /// The sweep of every source that declares `sweep`, in source registration order.
  pub fn sweep_all(&self, scope: &SweepScope) -> anyhow::Result<Vec<SourceSweep>> {
    self
      .usage_sources
      .values()
      .filter_map(|source| source.implementation.as_ref())
      .filter(|implementation| implementation.ops().sweep)
      .map(|implementation| implementation.sweep(scope))
      .collect()
  }
}

/// One lifespan step: it runs with the feature context and resolves when the step is done. A failure stops startup or
/// is returned by the step runner.
pub type StepFn = fn(Arc<FeatureContext>) -> BoxFuture<'static, anyhow::Result<()>>;

impl Registry {
  /// Register a pre-recovery step (for example the session cgroup sweep). Steps run in registration order.
  pub fn pre_recovery_step(&mut self, name: &'static str, step: StepFn) -> Result<(), RegistryError> {
    let package = self.owner()?;
    self.pre_recovery_steps.insert(package, name, step)
  }

  /// Register a recovery step. Steps run in registration order, each in its own task.
  pub fn recovery_step(&mut self, name: &'static str, step: StepFn) -> Result<(), RegistryError> {
    let package = self.owner()?;
    self.recovery_steps.insert(package, name, step)
  }

  /// Register the trigger step, which runs after recovery and before the ready services. At most one registers it.
  pub fn trigger_step(&mut self, name: &'static str, step: StepFn) -> Result<(), RegistryError> {
    let package = self.owner()?;
    self.trigger_step.insert_single(package, name, step)
  }

  /// Register a let-go step. Normal stops run it after the shutdown log; failed startup runs it during cleanup.
  pub fn let_go_step(&mut self, name: &'static str, step: StepFn) -> Result<(), RegistryError> {
    let package = self.owner()?;
    self.let_go_steps.insert(package, name, step)
  }
}

impl Registry {
  /// The pre-recovery steps in registration order.
  pub(crate) fn pre_recovery_steps(&self) -> Vec<(&'static str, StepFn)> {
    self.pre_recovery_steps.pairs().map(|(name, step)| (name, *step)).collect()
  }

  /// The recovery steps in registration order.
  pub(crate) fn recovery_steps(&self) -> Vec<(&'static str, StepFn)> {
    self.recovery_steps.pairs().map(|(name, step)| (name, *step)).collect()
  }

  /// The trigger step, if one is registered.
  pub(crate) fn trigger_entry(&self) -> Option<(&'static str, StepFn)> {
    self.trigger_step.pairs().next().map(|(name, step)| (name, *step))
  }

  /// The let-go steps in registration order.
  pub(crate) fn let_go_steps(&self) -> Vec<(&'static str, StepFn)> {
    self.let_go_steps.pairs().map(|(name, step)| (name, *step)).collect()
  }
}

/// The handles a feature uses at request time and in its services.
pub struct FeatureContext {
  /// `config.yaml`, reloaded when the file moves. A reload that fails keeps the previous config.
  pub config: Arc<HotReload<CharlieBotConfig>>,
  /// `credentials.yaml`, reloaded when the file moves.
  pub credentials: Arc<HotReload<Credentials>>,
  /// The sole owner of session metadata and chat-event writes.
  pub store: Arc<Store>,
  /// The finished registry, read-only.
  pub registry: Arc<Registry>,
  /// The typed startup state that lifespan steps and services fill and route handlers read.
  pub startup: Arc<StartupState>,
}

/// The typed startup state: one value per Rust type. Lifespan steps and services insert the values they own, and
/// route handlers read them. A second value of one type is an error.
pub struct StartupState {
  values: RwLock<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>,
  stop: StartupStop,
}

impl Default for StartupState {
  fn default() -> Self {
    Self { values: RwLock::new(HashMap::new()), stop: StartupStop::default() }
  }
}

/// A state value inserted twice, or required before any step inserted it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateError(pub String);

impl fmt::Display for StateError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(&self.0)
  }
}

impl std::error::Error for StateError {}

impl StartupState {
  /// Create startup state that shares the process's stop request.
  pub fn with_stop(stop: StartupStop) -> Self {
    Self { values: RwLock::new(HashMap::new()), stop }
  }

  /// Return the stop request shared by startup steps.
  pub fn stop(&self) -> StartupStop {
    self.stop.clone()
  }

  /// Store `value` as the one value of its type.
  pub fn insert<T: Any + Send + Sync>(&self, value: T) -> Result<(), StateError> {
    let mut values = self.values.write().expect("the startup state lock is never poisoned");
    if values.contains_key(&TypeId::of::<T>()) {
      return Err(StateError(format!("the startup state already holds {}", type_name::<T>())));
    }
    values.insert(TypeId::of::<T>(), Arc::new(value));
    Ok(())
  }

  /// The value of type `T`, or `None` when no step inserted one.
  pub fn get<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
    let values = self.values.read().expect("the startup state lock is never poisoned");
    values.get(&TypeId::of::<T>()).map(|value| {
      Arc::clone(value).downcast::<T>().unwrap_or_else(|_| unreachable!("the entry is keyed by its own type"))
    })
  }

  /// The value of type `T`; a missing value fails with the type's name.
  pub fn require<T: Any + Send + Sync>(&self) -> Result<Arc<T>, StateError> {
    self.get::<T>().ok_or_else(|| StateError(format!("the startup state holds no {}", type_name::<T>())))
  }
}

#[cfg(test)]
mod usage_tests {
  use std::sync::atomic::{AtomicUsize, Ordering};

  use super::*;
  use crate::usage::{QuotaAccount, UsageOps, UsageSourceImpl};

  struct QuotaSource {
    error: Option<&'static str>,
    calls: Arc<AtomicUsize>,
  }

  impl UsageSourceImpl for QuotaSource {
    fn ops(&self) -> UsageOps {
      UsageOps { quota_accounts: true, ..UsageOps::default() }
    }

    fn quota_accounts(&self, _cfg: &CharlieBotConfig) -> anyhow::Result<Vec<Arc<dyn QuotaAccount>>> {
      self.calls.fetch_add(1, Ordering::SeqCst);
      if let Some(error) = self.error {
        anyhow::bail!("{error}");
      }
      Ok(Vec::new())
    }
  }

  fn source(name: &'static str, error: Option<&'static str>, calls: Arc<AtomicUsize>) -> UsageSource {
    UsageSource {
      name,
      id_prefixes: &[],
      run_logs_only: false,
      implementation: Some(Arc::new(QuotaSource { error, calls })),
    }
  }

  #[test]
  fn quota_accounts_returns_the_first_source_error() {
    let first_calls = Arc::new(AtomicUsize::new(0));
    let second_calls = Arc::new(AtomicUsize::new(0));
    let mut registry = Registry::new();
    registry
      .usage_sources
      .insert("first-package", "first", source("first", Some("first source failed"), first_calls.clone()))
      .unwrap();
    registry.usage_sources.insert("second-package", "second", source("second", None, second_calls.clone())).unwrap();
    let home = tempfile::tempdir().unwrap();
    let cfg = registry.load_config(home.path()).unwrap();

    let error = registry.quota_accounts(&cfg).err().unwrap();

    assert_eq!(error.to_string(), "first source failed");
    assert_eq!(first_calls.load(Ordering::SeqCst), 1);
    assert_eq!(second_calls.load(Ordering::SeqCst), 0);
  }
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot, SlotField};

  use super::*;
  use axum::body::Body;
  use axum::response::Response;
  use futures_util::future::BoxFuture;

  fn no_routes(_features: &Arc<FeatureContext>) -> axum::Router {
    axum::Router::new()
  }

  fn no_path(_path: &str) -> bool {
    false
  }

  fn global(_features: &FeatureContext) -> Result<minijinja::Value, String> {
    Ok(minijinja::Value::from(true))
  }

  fn session_slot(owner: &'static str) -> MetadataSlot {
    MetadataSlot {
      owner,
      on: MetadataFile::Session,
      after: None,
      fields: vec![SlotField { name: owner, default: serde_json::Value::Null }],
    }
  }

  /// Registers under the package name `zeta`, which sorts after `alpha`: the tests check that the registration order,
  /// not the name order, drives the outputs.
  fn zeta(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.router(no_routes, no_path)?;
    registry.metadata_slot(session_slot("zeta"))?;
    registry.template_global("zeta_global", global)
  }

  fn alpha(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.router(no_routes, no_path)?;
    registry.metadata_slot(session_slot("alpha"))?;
    registry.template_global("alpha_global", global)
  }

  fn shared_global(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.template_global("shared", global)
  }

  fn websocket_handler(_features: Arc<FeatureContext>, _request: WebSocketRequest) -> BoxFuture<'static, Response> {
    Box::pin(async { Response::new(Body::empty()) })
  }

  fn websocket_alpha(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.websocket("/ws/test/*", websocket_handler)
  }

  fn websocket_zeta(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.websocket("/ws/test/*", websocket_handler)
  }

  /// A failed step reads as the package's failure, with every context frame of the chain in the message.
  #[test]
  fn a_failed_step_names_the_package_and_the_whole_chain() {
    use anyhow::Context;

    let step: anyhow::Result<()> = Err(anyhow::anyhow!("HOME must be set")).context("reading the host");
    let error = registration_step("src.backends.example", step).unwrap_err();
    assert_eq!(error.package, "src.backends.example");
    assert_eq!(error.message, "reading the host: HOME must be set");
  }

  #[test]
  fn registration_order_drives_routes_and_metadata_keys() {
    let mut registry = Registry::new();
    registry.register_package("src.features.zeta", zeta).unwrap();
    registry.register_package("src.features.alpha", alpha).unwrap();

    let routers: Vec<&str> = registry.routers().iter().map(|route| route.package).collect();
    assert_eq!(routers, ["src.features.zeta", "src.features.alpha"]);
    assert_eq!(registry.page_packages(), ["src.features.zeta", "src.features.alpha"]);
    let owners: Vec<&str> = registry.metadata_slots().iter().map(|slot| slot.owner).collect();
    assert_eq!(owners, ["zeta", "alpha"]);
    let globals: Vec<&str> = registry.template_globals().map(|(name, _)| name).collect();
    assert_eq!(globals, ["zeta_global", "alpha_global"]);
  }

  #[test]
  fn duplicate_registration_fails_and_names_both_packages() {
    let mut registry = Registry::new();
    registry.register_package("src.features.alpha", shared_global).unwrap();
    let error = registry.register_package("src.features.zeta", shared_global).unwrap_err();
    assert_eq!(error.package, "src.features.zeta");
    assert!(error.message.contains("src.features.alpha"), "{error}");
  }

  #[test]
  fn duplicate_websocket_pattern_fails_and_names_both_packages() {
    let mut registry = Registry::new();
    registry.register_package("src.features.alpha", websocket_alpha).unwrap();
    let error = registry.register_package("src.features.zeta", websocket_zeta).unwrap_err();
    assert_eq!(error.package, "src.features.zeta");
    assert!(error.to_string().contains("src.features.alpha"), "{error}");
    assert!(error.to_string().contains("src.features.zeta"), "{error}");
  }

  #[test]
  fn a_package_listed_twice_fails() {
    let mut registry = Registry::new();
    registry.register_package("src.features.alpha", alpha).unwrap();
    let error = registry.register_package("src.features.alpha", alpha).unwrap_err();
    assert_eq!(error.package, "src.features.alpha");
  }

  #[test]
  fn a_registration_outside_a_package_fails() {
    let mut registry = Registry::new();
    assert!(registry.template_global("loose", global).is_err());
  }
}
