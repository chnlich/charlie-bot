//! The registry value: one ledger per hook kind, filled while the packages register and read afterwards.

use std::fmt;
use std::path::Path;
use std::sync::{Arc, OnceLock, Weak};

use charliebot_core::config::{CharlieBotConfig, ConfigError, ConfigSchema, ConfigSection};
use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot, MetadataSlotValidator};
use charliebot_core::paths;

use crate::backend::{
  BackendLifecycle, BackendReadingLimitsRegistration, BackendTypeRegistration, ChildEnvFn, DefaultLifecycle,
  ReadingLimitsFn, UsageResolverConstructor,
};
use crate::context::{FeatureContext, StateError};
use crate::lifespan::StepFn;
use crate::pages::{HomeCard, TemplateGlobalFn};
use crate::sequence::SequenceController;
use crate::sidebar::SidebarContribution;
use crate::turn::TurnContribution;
use crate::usage::UsageSource;
use crate::wiring::{
  CheckFn, DiffRootFn, FileView, RouterRegistration, ScheduledHandlerFn, Service, StartupCheckFn, WebSocketHandler,
};

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
  current: Option<&'static str>,
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
  pub(crate) child_envs: Ledger<ChildEnvFn>,
  pub(crate) reading_limits: Ledger<ReadingLimitsFn>,
  pub(crate) backend_reading_limits: Vec<BackendReadingLimitsRegistration>,
  pub(crate) usage_resolvers: Ledger<UsageResolverConstructor>,
  pub(crate) sequence_controllers: Ledger<Arc<dyn SequenceController>>,
  /// The lifecycle of a backend type that registered none: one process per run.
  pub(crate) default_lifecycle: Arc<dyn BackendLifecycle>,
  pub(crate) pre_recovery_steps: Ledger<StepFn>,
  pub(crate) recovery_step: Ledger<StepFn>,
  pub(crate) trigger_step: Ledger<StepFn>,
  pub(crate) let_go_steps: Ledger<StepFn>,
  /// The server's feature context, bound once right after the server builds it. The store's after-turn hooks read it
  /// through this link. It is a `Weak`: the store's hooks hold the registry, and a strong link would form a cycle that
  /// keeps the context alive.
  feature_context: OnceLock<Weak<FeatureContext>>,
}

impl Default for Registry {
  fn default() -> Registry {
    Registry::new()
  }
}

impl Registry {
  /// An empty registry. The composition root registers each package into it.
  pub fn new() -> Registry {
    Registry {
      current: None,
      page_packages: Vec::new(),
      routers: Vec::new(),
      websocket_routes: Ledger::new("websocket route"),
      services: Ledger::new("service"),
      file_views: Ledger::new("file view"),
      startup_checks: Ledger::new("startup check"),
      config_checks: Ledger::new("config check"),
      config_sections: Ledger::new("config section"),
      config_legacy_keys: Ledger::new("legacy config key"),
      diff_roots: Ledger::new("diff root"),
      template_globals: Ledger::new("template global"),
      home_cards: Vec::new(),
      scheduled_handlers: Ledger::new("scheduled handler"),
      metadata_slots: Vec::new(),
      metadata_slot_validators: Vec::new(),
      turn_contributions: Ledger::new("turn contribution"),
      sidebar_contributions: Ledger::new("sidebar contribution"),
      usage_sources: Ledger::new("usage source"),
      usage_attributions: Ledger::new("usage attribution"),
      backend_types: Ledger::new("backend type"),
      child_envs: Ledger::new("child env function"),
      reading_limits: Ledger::new("reading limits"),
      backend_reading_limits: Vec::new(),
      usage_resolvers: Ledger::new("usage resolver"),
      sequence_controllers: Ledger::new("sequence controller"),
      default_lifecycle: Arc::new(DefaultLifecycle),
      pre_recovery_steps: Ledger::new("pre-recovery step"),
      recovery_step: Ledger::new("recovery step"),
      trigger_step: Ledger::new("trigger step"),
      let_go_steps: Ledger::new("let-go step"),
      feature_context: OnceLock::new(),
    }
  }

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
      child_envs,
      reading_limits,
      backend_reading_limits,
      usage_resolvers,
      sequence_controllers,
      default_lifecycle: _,
      pre_recovery_steps,
      recovery_step,
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
    inventory.push(format!("default_lifecycle\t-\t{}", std::any::type_name::<DefaultLifecycle>()));
    append_ledger_inventory(&mut inventory, pre_recovery_steps);
    append_ledger_inventory(&mut inventory, recovery_step);
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

#[cfg(test)]
mod tests {
  use std::fs;
  use std::sync::Arc;

  use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot, SlotField};

  use super::*;
  use crate::context::FeatureContext;
  use crate::pages;
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

  fn websocket_handler(
    _features: Arc<FeatureContext>,
    _request: crate::wiring::WebSocketRequest,
  ) -> BoxFuture<'static, Response> {
    Box::pin(async { Response::new(Body::empty()) })
  }

  fn websocket_alpha(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.websocket("/ws/test/*", websocket_handler)
  }

  fn websocket_zeta(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.websocket("/ws/test/*", websocket_handler)
  }

  #[test]
  fn registration_order_drives_routes_page_parts_and_metadata_keys() {
    let root = tempfile::tempdir().unwrap();
    for package in ["src.features.zeta", "src.features.alpha"] {
      let templates = pages::package_directory(root.path(), package).join("templates");
      let part = package.rsplit('.').next().unwrap();
      fs::create_dir_all(templates.join(part)).unwrap();
      fs::write(templates.join(part).join("page_head.html"), "").unwrap();
    }
    let mut registry = Registry::new();
    registry.register_package("src.features.zeta", zeta).unwrap();
    registry.register_package("src.features.alpha", alpha).unwrap();

    let routers: Vec<&str> = registry.routers().iter().map(|route| route.package).collect();
    assert_eq!(routers, ["src.features.zeta", "src.features.alpha"]);
    let parts = pages::page_parts(root.path(), registry.page_packages(), "head").unwrap();
    assert_eq!(parts, ["zeta/page_head.html", "alpha/page_head.html"]);
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
