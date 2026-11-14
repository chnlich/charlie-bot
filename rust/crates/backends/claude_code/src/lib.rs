//! The cc-claude option, factory, lifecycle, environment, event translation, and the Claude Code usage source.

pub mod claude_accounts;
pub mod claude_compaction;
pub mod claude_config;
pub mod claude_idle_compaction;
pub mod claude_launch;
pub mod claude_lifecycle;
pub mod claude_metadata;
pub mod claude_relay;
pub mod event_types;
pub mod factory;
pub mod login_dirs;
pub mod master_cc_relay;
pub mod midturn_delivery;
pub mod options;
pub mod setup_check;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
pub mod turn_contribution;
pub mod usage_logs;
pub mod usage_quota;
pub mod usage_sweep;

#[path = "claude_code.rs"]
mod backend;

pub use backend::{
  AnthropicEndpointBackend, CLAUDE_MODEL_ENV_NAMES, ClaudeCodeBackend, DISALLOWED_TOOLS_FLAG,
  EXCLUDE_DYNAMIC_SECTIONS_FLAG, GATEWAY_AUTH_TOKEN_ENV, GATEWAY_BASE_URL_ENV, GATEWAY_CUSTOM_HEADERS_ENV,
  GATEWAY_ENDPOINT_ENV_NAMES, GATEWAY_EXTRA_BODY_ENV, HEADLESS_DISALLOWED_TOOLS, INSTRUCTIONS_WRITTEN_EVENT,
  base_disallowed_tools, claude_child_env, claude_model_env, claude_reading_limits, claude_reading_limits_from,
  out_of_family_served_models, without_gateway_env,
};
pub use login_dirs::ClaudeHost;
pub use usage_logs::USAGE_SOURCE;

use std::sync::Arc;

use charliebot_registry::backend::{BackendLifecycle, BackendTraits, BackendTypeRegistration, ResumeStyle};
use charliebot_registry::usage::{CLAUDE_READING_KIND, UsageSource};
use charliebot_registry::{Registry, RegistryError, registration_step};
use serde_json::{Map, Value};

use self::claude_lifecycle::ClaudeCodeLifecycle;
use self::factory::ClaudeCodeFactory;
use self::options::option_model;
use self::usage_logs::ClaudeLogs;
use self::usage_quota::QuotaHttp;

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
    lifecycle: Some(Arc::new(ClaudeCodeLifecycle::with_state(Arc::clone(&state))) as Arc<dyn BackendLifecycle>),
    translate_fallback: true,
  })?;
  registry.option_schema("cc-claude", options::SCHEMA)?;
  registry.backend_cli_binary("cc-claude", options::cli_binary)?;
  // The Auto set reads this member through the same shared observations the pool selects logins from.
  let reading_state = Arc::clone(&state);
  registry.member_reading(
    "cc-claude",
    Arc::new(move |cfg, option, now| claude_accounts::member_reading(&reading_state, cfg, option, now)),
  )?;
  registry.config_section(charliebot_core::config::ConfigSection {
    key: "accounts",
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
    after: Some("exit_code"),
    fields: vec![charliebot_core::metadata_slot::SlotField {
      name: "claude_session_id",
      default: serde_json::Value::Null,
    }],
  })?;
  registry.child_env("claude_child_env", claude_child_env)?;
  registry.reading_limits(CLAUDE_READING_KIND, claude_reading_limits)?;
  registry.usage_source(UsageSource {
    name: USAGE_SOURCE,
    id_prefixes: &["claude-"],
    run_logs_only: false,
    implementation: Some(Arc::new(ClaudeLogs::new(host, Arc::clone(&state), Arc::new(http)))),
  })?;
  registry.attribute_backend_type("cc-claude", USAGE_SOURCE)?;
  registry.config_check("check_claude_pools", claude_config::check_claude_pools)?;
  registry.turn_contribution("claude_code", Arc::new(turn_contribution::ClaudeCodeTurnContribution))
}

fn validate_claude_session_fields(values: &mut Map<String, Value>) -> Option<String> {
  values
    .get("claude_account")
    .is_some_and(|value| !value.is_null() && !value.is_string())
    .then(|| "claude_account must be a string or null".to_string())
}

#[cfg(test)]
mod tests {
  use std::path::PathBuf;

  use charliebot_registry::usage::UsageOps;

  use super::test_support::{register_synthetic, synthetic_host};
  use super::*;

  #[test]
  fn registration_matches_the_usage_source_and_attribution() {
    let mut registry = Registry::new();
    registry.register_package(PACKAGE, register_synthetic).unwrap();

    let source = registry.usage_source_for("cc-claude").unwrap().unwrap();
    assert_eq!(source.name, "Claude Code");
    assert_eq!(source.id_prefixes, &["claude-"]);
    assert!(!source.run_logs_only);
    assert_eq!(
      source.implementation.as_ref().unwrap().ops(),
      UsageOps { logs: true, read: true, live_cli_sessions: false, quota_accounts: true, sweep: true }
    );
  }

  #[test]
  fn host_expands_only_a_leading_tilde() {
    let host = synthetic_host();
    assert_eq!(host.expand("~/accounts/a"), PathBuf::from("/synthetic/home/accounts/a"));
    assert_eq!(host.expand("~"), PathBuf::from("/synthetic/home"));
    assert_eq!(host.expand("~other/a"), PathBuf::from("~other/a"));
    assert_eq!(host.expand("/abs/a"), PathBuf::from("/abs/a"));
    assert_eq!(host.default_dir(), PathBuf::from("/synthetic/home/.claude"));
  }
}
