//! The `charlie-code` backend type (`src/backends/charlie_code`): the CharlieCode CLI against an OpenAI-compatible
//! endpoint. CLC's usage lives only in CharlieBot's own run logs, so its usage source has no log reader.

mod charlie_code_cli;
pub mod factory;
pub mod options;
mod translate;

use std::sync::Arc;

use charliebot_registry::backend::{BackendTraits, BackendTypeRegistration, ResumeStyle};
use charliebot_registry::usage::UsageSource;
use charliebot_registry::{Registry, RegistryError};

use charliebot_agents::cli_base::CliHost;

/// The ledger's source value and the usage page's card title for Charlie Code's usage.
pub const USAGE_SOURCE: &str = "CLC";

const PACKAGE: &str = "src.backends.charlie_code";

/// Registers the `charlie-code` type and the CLC usage source, which attributes the type's usage.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  let host =
    CliHost::from_process().map_err(|error| RegistryError { package: PACKAGE, message: format!("{error:#}") })?;
  register_with_host(registry, host)
}

/// [`register`] over an explicit host.
pub fn register_with_host(registry: &mut Registry, host: CliHost) -> Result<(), RegistryError> {
  registry.backend_type(BackendTypeRegistration {
    type_name: "charlie-code",
    traits: BackendTraits {
      resume: ResumeStyle::NativeId,
      restart_reattach: true,
      preassigned_session_id: false,
      family_prefix: None,
      manual_compact: false,
      compact_hint: None,
    },
    option_model: options::option_model,
    factory: Arc::new(factory::CharlieCodeFactory::new(host)),
    lifecycle: None,
    translate_fallback: false,
  })?;
  registry.option_schema("charlie-code", options::SCHEMA)?;
  registry.usage_source(UsageSource {
    name: USAGE_SOURCE,
    id_prefixes: &["charlie-code-", "clc-"],
    run_logs_only: true,
    implementation: None,
  })?;
  registry.attribute_backend_type("charlie-code", USAGE_SOURCE)
}
