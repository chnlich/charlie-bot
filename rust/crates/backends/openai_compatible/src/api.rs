//! The public entry of the `openai_compatible` package: its package label and the registration that
//! `app/packages` calls.

use std::sync::Arc;

use charliebot_core::backend_types;
use charliebot_registry::backend::{BackendLifecycle, BackendTraits, BackendTypeRegistration, ResumeStyle};
use charliebot_registry::{Registry, RegistryError};

use charliebot_claude_code::USAGE_SOURCE;
use charliebot_claude_code::claude_lifecycle::ClaudeCliLifecycle;

use crate::anthropic_proxy;
use crate::factory;
use crate::options;

/// The package label the composition list registers this backend's package under.
pub const PACKAGE: &str = "backends.openai_compatible";

/// The registration of the proxy route, the `cc-openai-compatible` type and its usage attribution.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router(anthropic_proxy::router, anthropic_proxy::matches)?;
  registry.backend_type(BackendTypeRegistration {
    type_name: backend_types::CC_OPENAI_COMPATIBLE,
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
  registry.option_schema(backend_types::CC_OPENAI_COMPATIBLE, options::SCHEMA)?;
  registry.attribute_backend_type(backend_types::CC_OPENAI_COMPATIBLE, USAGE_SOURCE)
}
