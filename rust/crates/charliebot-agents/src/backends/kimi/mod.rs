//! The `cc-kimi` backend type (`src/backends/kimi`): Claude Code against Kimi's Anthropic-compatible endpoint. The Claude
//! CLI runs every call, so the type's usage counts under Claude Code.

pub mod factory;
mod kimi_claude;
mod options;

use std::sync::Arc;

use charliebot_registry::backend::{BackendLifecycle, BackendTraits, BackendTypeRegistration, ResumeStyle};
use charliebot_registry::{Registry, RegistryError};

use super::claude_code::USAGE_SOURCE;
use super::claude_code::claude_lifecycle::ClaudeCliLifecycle;

/// Registers the `cc-kimi` type and its usage attribution.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.backend_type(BackendTypeRegistration {
    type_name: "cc-kimi",
    traits: BackendTraits {
      resume: ResumeStyle::CliFlag,
      restart_reattach: true,
      preassigned_session_id: false,
      family_prefix: None,
      manual_compact: false,
      compact_hint: None,
    },
    option_model: options::option_model,
    factory: Arc::new(factory::KimiFactory),
    lifecycle: Some(Arc::new(ClaudeCliLifecycle) as Arc<dyn BackendLifecycle>),
    translate_fallback: false,
  })?;
  registry.attribute_backend_type("cc-kimi", USAGE_SOURCE)
}
