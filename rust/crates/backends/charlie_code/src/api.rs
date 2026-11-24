//! The public entry of the `charlie-code` package: its package label, its backend type name, its usage source and
//! the two registrations that `app/packages` and the tests call.

use std::sync::Arc;

use charliebot_core::backend_types;
use charliebot_registry::backend::{BackendTraits, BackendTypeRegistration, ResumeStyle};
use charliebot_registry::usage::{NativeCacheRead, UsageSource};
use charliebot_registry::{Registry, RegistryError, registration_step};

use charliebot_agents::cli_base::CliHost;

use crate::{factory, options};

/// The ledger's source value and the usage page's card title for Charlie Code's usage.
pub const USAGE_SOURCE: &str = "CLC";

/// The package label the composition list registers this backend's package under.
pub const PACKAGE: &str = "backends.charlie_code";

/// The backend type this package registers: the `type` a `backends.options` entry names.
pub const TYPE_NAME: &str = backend_types::CHARLIE_CODE;

/// The registration of the `charlie-code` type and the CLC usage source, which attributes the type's usage.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  register_with_host(registry, registration_step(PACKAGE, CliHost::from_process())?)
}

/// [`register`] over an explicit host.
pub fn register_with_host(registry: &mut Registry, host: CliHost) -> Result<(), RegistryError> {
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
    factory: Arc::new(factory::CharlieCodeFactory::new(host)),
    lifecycle: None,
    translate_fallback: false,
  })?;
  registry.option_schema(TYPE_NAME, options::SCHEMA)?;
  registry.usage_source(UsageSource {
    name: USAGE_SOURCE,
    id_prefixes: &["charlie-code-", "clc-"],
    run_logs_only: true,
    native_input: charliebot_registry::usage::NativeInput::Inclusive,
    native_scope: charliebot_registry::usage::NativeScope::PerRun,
    native_cache_read: NativeCacheRead::CachedTokens,
    implementation: None,
  })?;
  registry.attribute_backend_type(TYPE_NAME, USAGE_SOURCE)
}
