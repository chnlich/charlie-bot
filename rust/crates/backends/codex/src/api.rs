//! The public entry of the `codex` package: its package label and the two registrations that `app/packages` and the
//! tests call.

use std::sync::Arc;

use charliebot_core::backend_types;
use charliebot_registry::backend::{BackendTraits, BackendTypeRegistration, ResumeStyle};
use charliebot_registry::usage::{NativeCacheRead, UsageSource};
use charliebot_registry::{Registry, RegistryError, registration_step};

use crate::factory::CodexFactory;
use crate::host::{CodexHost, codex_home};
use crate::options;
use crate::resolver;
use crate::rollout;
use crate::usage_logs::{self, USAGE_SOURCE};
use crate::usage_quota;

/// The package label the composition list registers this backend's package under.
pub const PACKAGE: &str = "backends.codex";

/// The registration of the codex backend type, its usage resolver, its usage source, and the source's attribution.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  let host = registration_step(PACKAGE, CodexHost::from_process())?;
  register_with_host(registry, &host)
}

/// [`register`] over an explicit host.
pub fn register_with_host(registry: &mut Registry, host: &CodexHost) -> Result<(), RegistryError> {
  registry.backend_type(BackendTypeRegistration {
    type_name: backend_types::CODEX,
    traits: BackendTraits {
      resume: ResumeStyle::NativeId,
      restart_reattach: true,
      preassigned_session_id: false,
      family_prefix: Some("codex"),
      manual_compact: false,
      compact_hint: Some("codex only compacts automatically — tune model_auto_compact_token_limit"),
    },
    option_model: options::option_model,
    factory: Arc::new(CodexFactory::new(host.clone())),
    lifecycle: None,
    translate_fallback: false,
  })?;
  registry.option_schema(backend_types::CODEX, options::SCHEMA)?;
  let sessions_dir = codex_home(host).join(rollout::CODEX_SESSIONS_DIR_NAME);
  registry.member_reading(
    backend_types::CODEX,
    Arc::new(move |_cfg, _option, now| usage_quota::member_reading(&sessions_dir, now)),
  )?;
  let codex_home_dir = codex_home(host);
  registry.usage_resolver(
    backend_types::CODEX,
    Arc::new(move |usage_host| resolver::usage_resolver(codex_home_dir.clone(), usage_host)),
  )?;
  registry.usage_source(UsageSource {
    name: USAGE_SOURCE,
    id_prefixes: &["codex-"],
    run_logs_only: false,
    native_input: charliebot_registry::usage::NativeInput::Inclusive,
    native_scope: charliebot_registry::usage::NativeScope::PerRun,
    native_cache_read: NativeCacheRead::CachedTokens,
    implementation: Some(Arc::new(usage_logs::CodexLogs::new(codex_home(host)))),
  })?;
  registry.attribute_backend_type(backend_types::CODEX, USAGE_SOURCE)?;
  Ok(())
}
