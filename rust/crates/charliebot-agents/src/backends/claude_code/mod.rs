//! The cc-claude option, factory, lifecycle, environment, and event translation.

pub mod claude_launch;
pub mod claude_lifecycle;
pub mod event_types;
pub mod factory;
pub mod login_dirs;
pub mod options;

#[path = "claude_code.rs"]
mod backend;

pub use backend::{
  ClaudeCodeBackend, DISALLOWED_TOOLS_FLAG, HEADLESS_DISALLOWED_TOOLS, INSTRUCTIONS_FILE_NAME,
  INSTRUCTIONS_WRITTEN_EVENT, SKIP_PERMISSIONS_FLAG, claude_child_env, claude_reading_limits,
  claude_reading_limits_from, headless_claude_env, out_of_family_served_models,
};

use std::sync::Arc;

use charliebot_registry::Registry;
use charliebot_registry::backend::{BackendLifecycle, BackendTraits, BackendTypeRegistration, ResumeStyle};

use self::claude_lifecycle::ClaudeCodeLifecycle;
use self::factory::ClaudeCodeFactory;
use self::options::option_model;

/// Register cc-claude, its config section, metadata fields, child environment edit, and reading limits.
pub fn register(registry: &mut Registry) -> Result<(), charliebot_registry::RegistryError> {
  registry.backend_type(BackendTypeRegistration {
    type_name: "cc-claude",
    traits: BackendTraits {
      resume: ResumeStyle::CliFlag,
      restart_reattach: true,
      preassigned_session_id: true,
      family_prefix: None,
      manual_compact: true,
      compact_hint: None,
    },
    option_model,
    factory: Arc::new(ClaudeCodeFactory),
    lifecycle: Some(Arc::new(ClaudeCodeLifecycle::new()) as Arc<dyn BackendLifecycle>),
    translate_fallback: true,
  })?;
  registry.config_section(charliebot_core::config::ConfigSection {
    key: "accounts",
    legacy_keys: &[("claude_accounts", "accounts.claude"), ("claude_compaction", "accounts.claude_compaction")],
    parse: raw_section,
  })?;
  registry.metadata_slot(charliebot_core::metadata_slot::MetadataSlot {
    owner: "claude_code",
    on: charliebot_core::metadata_slot::MetadataFile::Session,
    after: Some("cc_session_started_at"),
    fields: vec![charliebot_core::metadata_slot::SlotField {
      name: "claude_account",
      default: serde_json::Value::Null,
    }],
  })?;
  registry.metadata_slot(charliebot_core::metadata_slot::MetadataSlot {
    owner: "claude_code",
    on: charliebot_core::metadata_slot::MetadataFile::Thread,
    after: Some("exit_code"),
    fields: vec![charliebot_core::metadata_slot::SlotField {
      name: "claude_session_id",
      default: serde_json::Value::Null,
    }],
  })?;
  registry.child_env("claude_child_env", claude_child_env)?;
  registry.reading_limits("claude", claude_reading_limits)
}

fn raw_section(value: &serde_yaml::Value) -> Result<charliebot_core::config::SectionValue, String> {
  Ok(Arc::new(value.clone()))
}
