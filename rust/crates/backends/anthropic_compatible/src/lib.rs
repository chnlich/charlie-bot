//! Claude Code against a configured Anthropic-compatible endpoint.

pub mod factory;
pub mod options;
mod usage;

use std::sync::Arc;

use charliebot_registry::backend::{BackendLifecycle, BackendTraits, BackendTypeRegistration, ResumeStyle};
use charliebot_registry::{Registry, RegistryError};

use charliebot_claude_code::claude_lifecycle::ClaudeCliLifecycle;

const TYPE_NAME: &str = "cc-anthropic-compatible";
const READING_CLAUDE: &str = "claude";

/// Register Claude Code against an Anthropic-compatible endpoint.
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
  registry.backend_reading_limits(TYPE_NAME, READING_CLAUDE, usage::claude_reading_limits)?;
  registry.attribute_backend_type(TYPE_NAME, charliebot_claude_code::USAGE_SOURCE)
}
