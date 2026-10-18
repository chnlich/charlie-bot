//! The `src/backends` packages before their ports land: each backend type with its traits, a factory that refuses every
//! launch, and the data its `register` makes. The claude account label is the one lifecycle carried over, because the
//! store's anchor reconcile keeps it on every session.

use std::sync::Arc;

use anyhow::bail;
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_registry::backend::{
  AgentBackend, BackendFactory, BackendLifecycle, BackendTraits, BackendTypeRegistration, LaunchArgs, ResumeStyle,
};
use charliebot_registry::{Registry, RegistryError};

/// The traits of a type with no family, no manual compaction and no compact hint.
const fn plain(resume: ResumeStyle, restart_reattach: bool) -> BackendTraits {
  BackendTraits {
    resume,
    restart_reattach,
    preassigned_session_id: false,
    family_prefix: None,
    manual_compact: false,
    compact_hint: None,
  }
}

/// Registers one backend type. Its factory refuses every launch, because the backend is not ported.
fn register_type(
  registry: &mut Registry,
  type_name: &'static str,
  traits: BackendTraits,
  lifecycle: Option<Arc<dyn BackendLifecycle>>,
  translate_fallback: bool,
) -> Result<(), RegistryError> {
  registry.backend_type(BackendTypeRegistration {
    type_name,
    traits,
    option_model: accept_options,
    factory: Arc::new(NotPorted { type_name }),
    lifecycle,
    translate_fallback,
  })
}

/// The option model of a stub type: it accepts every type-specific key, as the skeleton's loader did.
fn accept_options(_type_fields: &serde_yaml::Mapping) -> Result<(), String> {
  Ok(())
}

/// The factory of a backend whose port has not landed.
struct NotPorted {
  type_name: &'static str,
}

impl BackendFactory for NotPorted {
  fn build(
    &self,
    _option: Option<&BackendOption>,
    _cfg: &CharlieBotConfig,
    _launch: LaunchArgs,
  ) -> anyhow::Result<Box<dyn AgentBackend>> {
    bail!("the {} backend is not ported", self.type_name)
  }
}

pub fn antigravity(registry: &mut Registry) -> Result<(), RegistryError> {
  register_type(registry, "antigravity", plain(ResumeStyle::NativeId, false), None, false)
}

pub fn charlie_code(registry: &mut Registry) -> Result<(), RegistryError> {
  register_type(registry, "charlie-code", plain(ResumeStyle::NativeId, true), None, false)
}

/// `src/backends/claude_code`: the registered backend, `accounts:` section and metadata fields.
pub fn claude_code(registry: &mut Registry) -> Result<(), RegistryError> {
  charliebot_agents::backends::claude_code::register(registry)
}

/// `src/backends/gemini`: the type, then its two retired secret keys.
pub fn gemini(registry: &mut Registry) -> Result<(), RegistryError> {
  register_type(registry, "gemini", plain(ResumeStyle::NativeId, true), None, false)?;
  registry
    .legacy_keys(&[("credentials: gemini_api_key", "gemini.api_key"), ("credentials: gemini_model", "gemini.model")])
}

pub fn kimi(registry: &mut Registry) -> Result<(), RegistryError> {
  register_type(registry, "cc-kimi", plain(ResumeStyle::CliFlag, true), None, false)
}

pub fn openai_compatible(registry: &mut Registry) -> Result<(), RegistryError> {
  register_type(registry, "cc-openai-compatible", plain(ResumeStyle::CliFlag, true), None, false)
}

pub fn opencode(registry: &mut Registry) -> Result<(), RegistryError> {
  register_type(registry, "opencode", plain(ResumeStyle::NativeId, false), None, false)
}
