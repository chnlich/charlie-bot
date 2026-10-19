//! The `gemini` backend type (`src/backends/gemini`): the gemini CLI in stream-json mode, and the two credential keys it
//! retired, which the config migrates to their replacements.

pub mod factory;
mod gemini_cli;
mod options;
mod translate;

use std::sync::Arc;

use charliebot_registry::backend::{BackendTraits, BackendTypeRegistration, ResumeStyle};
use charliebot_registry::{Registry, RegistryError};

use crate::cli_base::CliHost;

const PACKAGE: &str = "src.backends.gemini";

/// Registers the `gemini` type and its retired credential keys. As in Python, the type attributes no usage source.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  let host =
    CliHost::from_process().map_err(|error| RegistryError { package: PACKAGE, message: format!("{error:#}") })?;
  register_with_host(registry, host)
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
  registry
    .legacy_keys(&[("credentials: gemini_api_key", "gemini.api_key"), ("credentials: gemini_model", "gemini.model")])
}
