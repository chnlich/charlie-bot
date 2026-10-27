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
pub mod turn_contribution;
pub mod usage_logs;
pub mod usage_quota;
pub mod usage_sweep;

#[path = "claude_code.rs"]
mod backend;

pub use backend::{
  AnthropicEndpointBackend, ClaudeCodeBackend, DISALLOWED_TOOLS_FLAG, EXCLUDE_DYNAMIC_SECTIONS_FLAG,
  GATEWAY_AUTH_TOKEN_ENV, GATEWAY_BASE_URL_ENV, GATEWAY_CUSTOM_HEADERS_ENV, GATEWAY_ENDPOINT_ENV_NAMES,
  GATEWAY_EXTRA_BODY_ENV, HEADLESS_DISALLOWED_TOOLS, INSTRUCTIONS_FILE_NAME, INSTRUCTIONS_WRITTEN_EVENT,
  SKIP_PERMISSIONS_FLAG, claude_child_env, claude_model_env, claude_reading_limits, claude_reading_limits_from,
  headless_claude_env, out_of_family_served_models, without_gateway_env,
};
pub use login_dirs::ClaudeHost;
pub use usage_logs::USAGE_SOURCE;

use std::sync::Arc;

use charliebot_registry::Registry;
use charliebot_registry::RegistryError;
use charliebot_registry::backend::{BackendLifecycle, BackendTraits, BackendTypeRegistration, ResumeStyle};
use charliebot_registry::usage::UsageSource;
use serde_json::{Map, Value};

use self::claude_lifecycle::ClaudeCodeLifecycle;
use self::factory::ClaudeCodeFactory;
use self::options::option_model;
use self::usage_logs::ClaudeLogs;
use self::usage_quota::QuotaHttp;

const PACKAGE: &str = "src.backends.claude_code";

/// Register cc-claude, its Claude Code usage source, config section, metadata fields, child environment edit, and
/// reading limits.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  let host =
    ClaudeHost::from_process().map_err(|error| RegistryError { package: PACKAGE, message: format!("{error:#}") })?;
  let http =
    QuotaHttp::from_process().map_err(|error| RegistryError { package: PACKAGE, message: format!("{error:#}") })?;
  register_with_host(registry, host, http)
}

/// [`register`] over an explicit host and HTTP client.
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
  registry.config_section(charliebot_core::config::ConfigSection {
    key: "accounts",
    legacy_keys: &[("claude_accounts", "accounts.claude"), ("claude_compaction", "accounts.claude_compaction")],
    parse: claude_config::parse_accounts,
  })?;
  registry.metadata_slot_with_validator(
    charliebot_core::metadata_slot::MetadataSlot {
      owner: "claude_code",
      on: charliebot_core::metadata_slot::MetadataFile::Session,
      after: Some("cc_session_started_at"),
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
  registry.reading_limits("claude", claude_reading_limits)?;
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

/// Helpers for the unit tests of the Claude usage source: a synthetic host, and a config loaded through the registry.
#[cfg(test)]
pub(crate) mod test_support {
  use std::fs;
  use std::path::{Path, PathBuf};

  use charliebot_compat::http_client::{self, Settings};
  use charliebot_core::config::CharlieBotConfig;

  use super::*;

  pub fn synthetic_host() -> ClaudeHost {
    ClaudeHost { home: PathBuf::from("/synthetic/home"), claude_config_dir: PathBuf::from("/synthetic/home/.claude") }
  }

  /// An HTTP client that reaches no host: the unit tests never send a request through it.
  pub fn quota_http() -> QuotaHttp {
    let client = http_client::client(&Settings::default()).expect("the default client builds");
    QuotaHttp::with_endpoints(client, usage_quota::USAGE_URL, usage_quota::TOKEN_REFRESH_URL)
  }

  pub fn register_synthetic(registry: &mut Registry) -> Result<(), RegistryError> {
    register_with_host(registry, synthetic_host(), quota_http())
  }

  /// Loads `home/config.yaml` through the registry, with the `accounts.claude` pool and the worktree directory.
  pub fn load_config(home: &Path, accounts: &[(&str, &Path)], worktree_dir: &Path) -> CharlieBotConfig {
    let rows = if accounts.is_empty() {
      "  claude: []\n".to_string()
    } else {
      let mut rows = String::from("  claude:\n");
      for (label, dir) in accounts {
        rows.push_str(&format!("    - label: {label}\n      config_dir: '{}'\n", dir.display()));
      }
      rows
    };
    let text = format!("paths:\n  worktree_dir: '{}'\naccounts:\n{rows}", worktree_dir.display());
    fs::create_dir_all(home).expect("the synthetic home exists");
    fs::write(home.join("config.yaml"), text).expect("the synthetic config is written");
    let mut registry = Registry::new();
    registry.register_package(PACKAGE, register_synthetic).expect("the Claude package registers");
    registry.load_config(home).expect("the synthetic config loads")
  }
}

#[cfg(test)]
mod tests {
  use std::path::PathBuf;

  use charliebot_registry::usage::UsageOps;

  use super::test_support::{register_synthetic, synthetic_host};
  use super::*;

  #[test]
  fn registration_matches_the_python_usage_source_and_attribution() {
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
