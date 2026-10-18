//! The `src/backends` packages before their ports land: each backend type with its traits, a factory that refuses every
//! launch, and the data its `register` makes. The claude account label is the one lifecycle carried over, because the
//! store's anchor reconcile keeps it on every session.

use std::sync::Arc;

use anyhow::bail;
use charliebot_core::config::{BackendOption, CharlieBotConfig, ConfigSection};
use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot, SlotField};
use charliebot_registry::backend::{
  AgentBackend, BackendFactory, BackendLifecycle, BackendTraits, BackendTypeRegistration, Launch, LaunchArgs,
  LaunchContext, LaunchRefused, ResumeStyle,
};
use charliebot_registry::{Registry, RegistryError};
use charliebot_store::metadata::SessionMetadata;
use futures_util::future::BoxFuture;
use serde_json::Value;

use super::raw_section;

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

/// The lifecycle of the claude backend's account label: the label lives on the session as `claude_account`.
struct ClaudeAccountLifecycle;

impl BackendLifecycle for ClaudeAccountLifecycle {
  fn place<'a>(&'a self, _ctx: &'a LaunchContext) -> BoxFuture<'a, Result<Launch, LaunchRefused>> {
    Box::pin(async {
      Err(LaunchRefused {
        message: "the claude_code backend is not ported".to_string(),
        quota_exhausted: false,
        resets_at: None,
      })
    })
  }

  fn account_source(&self) -> &'static str {
    "claude_account"
  }

  fn account_label(&self, meta: &SessionMetadata) -> Option<String> {
    meta.extra("claude_account").and_then(Value::as_str).map(str::to_string)
  }

  fn record_account_label(&self, meta: &mut SessionMetadata, label: Option<&str>) -> bool {
    let value = label.map_or(Value::Null, |label| Value::String(label.to_string()));
    if meta.extra("claude_account") == Some(&value) {
      return false;
    }
    meta.set_extra("claude_account", value);
    true
  }
}

pub fn antigravity(registry: &mut Registry) -> Result<(), RegistryError> {
  register_type(registry, "antigravity", plain(ResumeStyle::NativeId, false), None, false)
}

pub fn charlie_code(registry: &mut Registry) -> Result<(), RegistryError> {
  register_type(registry, "charlie-code", plain(ResumeStyle::NativeId, true), None, false)
}

/// `src/backends/claude_code`: the translate-fallback type, its `accounts:` section and its session and thread fields.
pub fn claude_code(registry: &mut Registry) -> Result<(), RegistryError> {
  let traits = BackendTraits {
    resume: ResumeStyle::CliFlag,
    restart_reattach: true,
    preassigned_session_id: true,
    family_prefix: None,
    manual_compact: true,
    compact_hint: None,
  };
  register_type(registry, "cc-claude", traits, Some(Arc::new(ClaudeAccountLifecycle)), true)?;
  registry.config_section(ConfigSection {
    key: "accounts",
    legacy_keys: &[("claude_accounts", "accounts.claude"), ("claude_compaction", "accounts.claude_compaction")],
    parse: raw_section,
  })?;
  registry.metadata_slot(MetadataSlot {
    owner: "claude_code",
    on: MetadataFile::Session,
    after: Some("cc_session_started_at"),
    fields: vec![SlotField { name: "claude_account", default: Value::Null }],
  })?;
  registry.metadata_slot(MetadataSlot {
    owner: "claude_code",
    on: MetadataFile::Thread,
    after: Some("exit_code"),
    fields: vec![SlotField { name: "claude_session_id", default: Value::Null }],
  })
}

pub fn codex(registry: &mut Registry) -> Result<(), RegistryError> {
  let traits = BackendTraits {
    family_prefix: Some("codex"),
    compact_hint: Some("codex only compacts automatically — tune model_auto_compact_token_limit"),
    ..plain(ResumeStyle::NativeId, true)
  };
  register_type(registry, "codex", traits, None, false)
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
