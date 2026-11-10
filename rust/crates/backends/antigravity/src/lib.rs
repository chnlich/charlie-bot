//! The `antigravity` backend type: `agy --print`, whose JSON envelope the runner reads once
//! the process has exited. Token usage comes from CharlieBot's own run logs; quota-panel readings come from the CLI's
//! `/usage` command.

mod antigravity_cli;
mod envelope;
pub mod factory;
mod options;
mod usage_quota;

use std::sync::Arc;

use charliebot_registry::backend::{BackendTraits, BackendTypeRegistration, ResumeStyle};
use charliebot_registry::usage::UsageSource;
use charliebot_registry::{Registry, RegistryError, registration_step};

use charliebot_agents::cli_base::CliHost;

/// The backend type this package registers: the `type` a `backends.options` entry names.
pub use options::TYPE_NAME;

/// The ledger's source value and the usage page's card title for antigravity's usage.
pub const USAGE_SOURCE: &str = "Antigravity";

/// The package label the composition list registers this backend's package under.
pub const PACKAGE: &str = "backends.antigravity";

/// The `antigravity` package registration: its backend type and the usage source that attributes the type's usage.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  register_with_host(registry, registration_step(PACKAGE, CliHost::from_process())?)
}

/// [`register`] over an explicit host.
pub fn register_with_host(registry: &mut Registry, host: CliHost) -> Result<(), RegistryError> {
  let state = Arc::new(usage_quota::AntigravityQuotaState::default());
  let account = Arc::new(usage_quota::AntigravityQuotaAccount::new(host.clone(), Arc::clone(&state)));
  registry.backend_type(BackendTypeRegistration {
    type_name: TYPE_NAME,
    traits: BackendTraits {
      resume: ResumeStyle::NativeId,
      restart_reattach: true,
      preassigned_session_id: false,
      family_prefix: None,
      manual_compact: false,
      compact_hint: None,
    },
    option_model: options::option_model,
    factory: Arc::new(factory::AntigravityFactory::new(host)),
    lifecycle: None,
    translate_fallback: false,
  })?;
  registry.option_schema(TYPE_NAME, options::SCHEMA)?;
  registry.member_reading(TYPE_NAME, usage_quota::AntigravityQuotaState::member_reading(Arc::clone(&state)))?;
  registry.usage_source(UsageSource {
    name: USAGE_SOURCE,
    id_prefixes: &["antigravity-", "agy-"],
    run_logs_only: true,
    implementation: Some(Arc::new(usage_quota::AntigravityUsageSource::new(account))),
  })?;
  registry.attribute_backend_type(TYPE_NAME, USAGE_SOURCE)
}
