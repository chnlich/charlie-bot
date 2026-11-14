//! Claude Code against a configured Anthropic-compatible endpoint.

pub mod factory;
pub mod options;
mod usage;

pub use options::TYPE_NAME;

use std::sync::Arc;

use charliebot_registry::backend::{BackendLifecycle, BackendTraits, BackendTypeRegistration, ResumeStyle};
use charliebot_registry::usage::CLAUDE_READING_KIND;
use charliebot_registry::{Registry, RegistryError};

use charliebot_claude_code::claude_lifecycle::ClaudeCliLifecycle;

/// The package label the composition list registers this backend's package under.
pub const PACKAGE: &str = "backends.anthropic_compatible";

/// The `cc-anthropic-compatible` type's registration: its backend type, option schema, reading limits, host alias and
/// usage attribution.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.backend_type(BackendTypeRegistration {
    type_name: TYPE_NAME,
    traits: BackendTraits {
      resume: ResumeStyle::CliFlag,
      restart_reattach: true,
      preassigned_session_id: false,
      family_prefix: None,
      manual_compact: false,
      compact_hint: None,
    },
    option_model: options::option_model,
    factory: Arc::new(factory::AnthropicCompatibleFactory),
    lifecycle: Some(Arc::new(ClaudeCliLifecycle) as Arc<dyn BackendLifecycle>),
    translate_fallback: false,
  })?;
  registry.option_schema(TYPE_NAME, options::SCHEMA)?;
  registry.backend_reading_limits(TYPE_NAME, CLAUDE_READING_KIND, usage::claude_reading_limits)?;
  registry.backend_host_alias(TYPE_NAME, options::host_auth_alias)?;
  registry.backend_cli_binary(TYPE_NAME, options::cli_binary)?;
  registry.attribute_backend_type(TYPE_NAME, charliebot_claude_code::USAGE_SOURCE)
}
