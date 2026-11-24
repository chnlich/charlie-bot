//! The public entry of the `gemini` package: its package label and the two registrations that `app/packages` and the
//! tests call.

use std::sync::Arc;

use charliebot_agents::cli_base::CliHost;
use charliebot_registry::backend::{BackendTraits, BackendTypeRegistration, ResumeStyle};
use charliebot_registry::{Registry, RegistryError, registration_step};

use crate::{factory, options};

/// The package label the composition list registers this backend's package under.
pub const PACKAGE: &str = "backends.gemini";

/// The registration of the `gemini` type and the two legacy credential keys. It names no usage source.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  register_with_host(registry, registration_step(PACKAGE, CliHost::from_process())?)
}

/// [`register`] over an explicit host.
pub fn register_with_host(registry: &mut Registry, host: CliHost) -> Result<(), RegistryError> {
  registry.backend_type(BackendTypeRegistration {
    type_name: "gemini",
    traits: BackendTraits {
      resume: ResumeStyle::NativeId,
      restart_reattach: true,
      preassigned_session_id: false,
      family_prefix: None,
      manual_compact: false,
      compact_hint: None,
    },
    option_model: options::option_model,
    factory: Arc::new(factory::GeminiFactory::new(host)),
    lifecycle: None,
    translate_fallback: false,
  })?;
  registry.option_schema("gemini", options::SCHEMA)?;
  registry
    .legacy_keys(&[("credentials: gemini_api_key", "gemini.api_key"), ("credentials: gemini_model", "gemini.model")])
}
