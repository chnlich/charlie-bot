//! Backend runtime contracts and lifecycle registrations.

use std::any::type_name;
use std::sync::Arc;

pub use crate::registry::{
  AgentBackend, BackendFactory, BackendLifecycle, BackendReadingLimitsFn, BackendTraits, BackendTypeRegistration,
  ChildEnvFn, CompactionOutcome, CompactionRun, CompactionRunFn, ContextLimits, ContextStateFn, EmitFn, Env,
  ExitedProbe, FinalStdoutOutcome, IdleArmContext, IdleStartGate, Launch, LaunchArgs, LaunchContext, LaunchKind,
  LaunchRefused, LaunchWatch, OptionModel, ReadingLimitsFn, RecordAccountFn, Registry, ResumeStyle, RunningTurnsFn,
  ServeTransport, ServeTurn, ServeTurnArgs, ServeTurnResult, SessionContextStateFn, Translate, TransportKind,
  UsageResolver, UsageResolverConstructor, UsageResolverHost,
};

/// The lifecycle of a backend type that registered none.
pub struct DefaultLifecycle;

impl BackendLifecycle for DefaultLifecycle {}

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
      websocket_routes: crate::registry::Ledger::new("websocket route"),
      services: crate::registry::Ledger::new("service"),
      file_views: crate::registry::Ledger::new("file view"),
      startup_checks: crate::registry::Ledger::new("startup check"),
      config_checks: crate::registry::Ledger::new("config check"),
      config_sections: crate::registry::Ledger::new("config section"),
      config_legacy_keys: crate::registry::Ledger::new("legacy config key"),
      diff_roots: crate::registry::Ledger::new("diff root"),
      template_globals: crate::registry::Ledger::new("template global"),
      home_cards: Vec::new(),
      scheduled_handlers: crate::registry::Ledger::new("scheduled handler"),
      metadata_slots: Vec::new(),
      metadata_slot_validators: Vec::new(),
      turn_contributions: crate::registry::Ledger::new("turn contribution"),
      sidebar_contributions: crate::registry::Ledger::new("sidebar contribution"),
      usage_sources: crate::registry::Ledger::new("usage source"),
      usage_attributions: crate::registry::Ledger::new("usage attribution"),
      backend_types: crate::registry::Ledger::new("backend type"),
      child_envs: crate::registry::Ledger::new("child env function"),
      reading_limits: crate::registry::Ledger::new("reading limits"),
      backend_reading_limits: Vec::new(),
      usage_resolvers: crate::registry::Ledger::new("usage resolver"),
      sequence_controllers: crate::registry::Ledger::new("sequence controller"),
      default_lifecycle: Arc::new(DefaultLifecycle),
      default_lifecycle_name: type_name::<DefaultLifecycle>(),
      pre_recovery_steps: crate::registry::Ledger::new("pre-recovery step"),
      recovery_step: crate::registry::Ledger::new("recovery step"),
      trigger_step: crate::registry::Ledger::new("trigger step"),
      let_go_steps: crate::registry::Ledger::new("let-go step"),
      feature_context: std::sync::OnceLock::new(),
    }
  }
}
