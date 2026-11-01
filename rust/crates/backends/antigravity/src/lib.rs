//! The `antigravity` backend type (`src/backends/antigravity`): `agy --print`, whose JSON envelope the runner reads once
//! the process has exited. The CLI logs its usage only into CharlieBot's own run logs, so its usage source has no reader.

mod antigravity_cli;
pub mod factory;
mod options;

use std::sync::Arc;

use charliebot_registry::backend::{BackendTraits, BackendTypeRegistration, ResumeStyle};
use charliebot_registry::usage::UsageSource;
use charliebot_registry::{Registry, RegistryError, registration_step};

use charliebot_agents::cli_base::CliHost;

/// The ledger's source value and the usage page's card title for antigravity's usage.
pub const USAGE_SOURCE: &str = "Antigravity";

const PACKAGE: &str = "src.backends.antigravity";

/// Registers the `antigravity` type and the Antigravity usage source, which attributes the type's usage.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  register_with_host(registry, registration_step(PACKAGE, CliHost::from_process())?)
}

/// [`register`] over an explicit host.
pub fn register_with_host(registry: &mut Registry, host: CliHost) -> Result<(), RegistryError> {
  registry.backend_type(BackendTypeRegistration {
    type_name: "antigravity",
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
  registry.option_schema("antigravity", options::SCHEMA)?;
  registry.usage_source(UsageSource {
    name: USAGE_SOURCE,
    id_prefixes: &["antigravity-", "agy-"],
    run_logs_only: true,
    implementation: None,
  })?;
  registry.attribute_backend_type("antigravity", USAGE_SOURCE)
}
