//! The cc-claude registrations: the backend type with its lifecycle and option schema, the account and config sections,
//! the session and thread metadata fields, the child environment, the reading limits and the Claude Code usage source.

use std::sync::Arc;

use charliebot_core::backend_types;
use charliebot_registry::backend::{
  BackendLifecycle, BackendTraits, BackendTypeRegistration, CLAUDE_SESSION_ID_FIELD, ResumeStyle,
};
use charliebot_registry::usage::{CLAUDE_READING_KIND, NativeCacheRead, UsageSource};
use charliebot_registry::{Registry, RegistryError, registration_step};
use serde_json::{Map, Value};

use crate::claude_lifecycle::ClaudeCodeLifecycle;
use crate::factory::ClaudeCodeFactory;
use crate::options::option_model;
use crate::usage_logs::ClaudeLogs;
use crate::usage_quota::QuotaHttp;
use crate::{
  ClaudeHost, USAGE_SOURCE, claude_accounts, claude_child_env, claude_config, claude_reading_limits, options,
  turn_contribution,
};

/// The package label the composition list registers this backend's package under.
pub const PACKAGE: &str = "backends.claude_code";

/// Register cc-claude, its Claude Code usage source, config section, metadata fields, child environment edit, and
/// reading limits.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  let host = registration_step(PACKAGE, ClaudeHost::from_process())?;
  let http = registration_step(PACKAGE, QuotaHttp::from_process())?;
  register_with_host(registry, host, http)
}

/// The registration `register` performs, with the host and HTTP client supplied by the caller.
pub fn register_with_host(registry: &mut Registry, host: ClaudeHost, http: QuotaHttp) -> Result<(), RegistryError> {
  // The lifecycle, the quota accounts and the log source share one state value: the pool's readings and the panel's
  // readings are the same observations.
  let state = Arc::new(claude_accounts::ClaudeAccountState::new());
  registry.backend_type(BackendTypeRegistration {
    type_name: backend_types::CC_CLAUDE,
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
    lifecycle: Some(Arc::new(ClaudeCodeLifecycle::with_state(Arc::clone(&state))) as Arc<dyn BackendLifecycle>),
    translate_fallback: true,
  })?;
  registry.option_schema(backend_types::CC_CLAUDE, options::SCHEMA)?;
  // The Auto set reads this member through the same shared observations the pool selects logins from.
  let reading_state = Arc::clone(&state);
  registry.member_reading(
    backend_types::CC_CLAUDE,
    Arc::new(move |cfg, option, now| claude_accounts::member_reading(&reading_state, cfg, option, now)),
  )?;
  registry.config_section(charliebot_core::config::ConfigSection {
    key: claude_config::ACCOUNTS_SECTION_KEY,
    legacy_keys: &[("claude_accounts", "accounts.claude"), ("claude_compaction", "accounts.claude_compaction")],
    parse: claude_config::parse_accounts,
    dump: claude_config::dump_accounts,
  })?;
  registry.config_section(charliebot_core::config::ConfigSection {
    key: "claude_code",
    legacy_keys: &[],
    parse: claude_config::parse_claude_code,
    dump: claude_config::dump_claude_code,
  })?;
  registry.metadata_slot_with_validator(
    charliebot_core::metadata_slot::MetadataSlot {
      owner: "claude_code",
      on: charliebot_core::metadata_slot::MetadataFile::Session,
      after: Some(charliebot_store::metadata::CC_SESSION_STARTED_AT),
      fields: vec![charliebot_core::metadata_slot::SlotField {
        name: "claude_account",
        default: serde_json::Value::Null,
      }],
    },
    validate_claude_session_fields,
  )?;
  registry.metadata_slot(charliebot_core::metadata_slot::MetadataSlot {
    owner: "claude_code",
    on: charliebot_core::metadata_slot::MetadataFile::Thread,
    after: Some(charliebot_store::metadata::EXIT_CODE_FIELD),
    fields: vec![charliebot_core::metadata_slot::SlotField {
      name: CLAUDE_SESSION_ID_FIELD,
      default: serde_json::Value::Null,
    }],
  })?;
  registry.child_env("claude_child_env", claude_child_env)?;
  registry.reading_limits(CLAUDE_READING_KIND, claude_reading_limits)?;
  registry.usage_source(UsageSource {
    name: USAGE_SOURCE,
    id_prefixes: &["claude-"],
    run_logs_only: false,
    native_input: charliebot_registry::usage::NativeInput::Inclusive,
    native_scope: charliebot_registry::usage::NativeScope::PerRun,
    native_cache_read: NativeCacheRead::CachedTokens,
    implementation: Some(Arc::new(ClaudeLogs::new(host, Arc::clone(&state), Arc::new(http)))),
  })?;
  registry.attribute_backend_type(backend_types::CC_CLAUDE, USAGE_SOURCE)?;
  registry.config_check("check_claude_pools", claude_config::check_claude_pools)?;
  registry.turn_contribution("claude_code", Arc::new(turn_contribution::ClaudeCodeTurnContribution))
}

fn validate_claude_session_fields(values: &mut Map<String, Value>) -> Option<String> {
  values
    .get("claude_account")
    .is_some_and(|value| !value.is_null() && !value.is_string())
    .then(|| "claude_account must be a string or null".to_string())
}
