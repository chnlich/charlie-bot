//! The `cc-openai-compatible` backend type and the Anthropic proxy route its calls go through
//! (`src/backends/openai_compatible`). The Claude CLI runs every call, so the type's usage counts under Claude Code.

mod anthropic_proxy;
mod factory;
mod openai_compatible_claude;
mod options;
mod pyvalue;
mod sse;
mod translate;

use std::sync::Arc;

use charliebot_registry::backend::{BackendLifecycle, BackendTraits, BackendTypeRegistration, ResumeStyle};
use charliebot_registry::{Registry, RegistryError};

use charliebot_claude_code::claude_lifecycle::ClaudeCliLifecycle;

/// Registers the proxy route, the `cc-openai-compatible` type and its usage attribution.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router(anthropic_proxy::router, anthropic_proxy::matches)?;
  registry.backend_type(BackendTypeRegistration {
    type_name: "cc-openai-compatible",
    traits: BackendTraits {
      resume: ResumeStyle::CliFlag,
      restart_reattach: true,
      preassigned_session_id: false,
      family_prefix: None,
      manual_compact: false,
      compact_hint: None,
    },
    option_model: options::option_model,
    factory: Arc::new(factory::OpenAiCompatibleFactory),
    lifecycle: Some(Arc::new(ClaudeCliLifecycle) as Arc<dyn BackendLifecycle>),
    translate_fallback: false,
  })?;
  registry.attribute_backend_type("cc-openai-compatible", "Claude Code")
}
