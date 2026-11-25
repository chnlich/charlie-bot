//! The cc-claude option, factory, lifecycle, environment, event translation, and the Claude Code usage source.

pub mod account_labels;
pub mod claude_accounts;
pub mod claude_compaction;
pub mod claude_config;
pub mod claude_idle_compaction;
pub mod claude_launch;
pub mod claude_lifecycle;
pub mod claude_metadata;
pub mod claude_relay;
mod entry;
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
pub use entry::{PACKAGE, register, register_with_host};
pub use login_dirs::ClaudeHost;
pub use usage_logs::USAGE_SOURCE;

#[cfg(test)]
mod tests {
  use std::path::PathBuf;

  use charliebot_registry::Registry;
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
