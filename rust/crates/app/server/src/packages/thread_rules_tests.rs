//! Checks the manager prompt sources assembled for bound thread sessions.

use std::path::{Path, PathBuf};

use charliebot_core::RepoRoot;
use charliebot_core::test_support::synthetic_config;
use charliebot_store::metadata::{SessionMetadata, TaskProfile};
use charliebot_tasks::prompts::{PromptEnv, RuleInputs, SessionGoals, build_segments};
use serde_json::json;

use super::registry;

/// `rust/crates/app/server` sits four levels below the checkout root.
fn checkout_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(4).expect("the crate sits under the checkout").to_path_buf()
}

fn slack_thread_session() -> SessionMetadata {
  let mut meta = SessionMetadata::new("synthetic-slack-session", "Synthetic Slack thread", TaskProfile::Manager);
  meta.set_extra(
    "slack_origin",
    json!({"team_id": "T0SYNTHETIC", "channel_id": "C0SYNTHETIC", "thread_ts": "1700000000.000100"}),
  );
  meta
}

fn discord_thread_session() -> SessionMetadata {
  let mut meta = SessionMetadata::new("synthetic-discord-session", "Synthetic Discord thread", TaskProfile::Manager);
  meta.set_extra(
    "discord_origin",
    json!({"guild_id": "G0SYNTHETIC", "parent_channel_id": "C0SYNTHETIC", "thread_id": "900000000000000030"}),
  );
  meta
}

fn feishu_thread_session() -> SessionMetadata {
  let mut meta = SessionMetadata::new("synthetic-feishu-session", "Synthetic Feishu thread", TaskProfile::Manager);
  meta.set_extra(
    "feishu_origin",
    json!({"tenant_key": "T0SYNTHETIC", "chat_id": "oc_0synthetic", "root_id": "om_0synthetic_root"}),
  );
  meta
}

/// The source refs of a manager turn, in the order they enter the prompt.
fn manager_turn_sources(meta: &SessionMetadata) -> Vec<String> {
  let registry = registry().expect("the listed packages register");
  let checkout = RepoRoot::new(Some(checkout_root().into_os_string())).expect("the checkout path is valid");
  let home = tempfile::tempdir().expect("temp home");
  let cfg = synthetic_config(home.path());
  let env = PromptEnv { cfg: &cfg, registry: &registry, checkout: &checkout };
  let inputs = RuleInputs { overlay: None, chain: &[], node_ref: None, goals: &SessionGoals::default() };
  let (segments, _overlay_error) =
    build_segments(&env, meta, "manager_turn", &inputs).expect("the manager turn assembles from the checkout");
  segments.iter().flat_map(|segment| segment.sources.iter().map(|source| source.source_ref.clone())).collect()
}

fn assert_thread_manager_sources(platform: &str, scope_source: &str, meta: &SessionMetadata) {
  let sources = manager_turn_sources(meta);
  let expected_sources = [
    "prompts/task_base.md",
    "prompts/master.md",
    "prompts/manager_workflows.md",
    "prompts/task_manager.md",
    "prompts/web_reply.md",
    "metadata.json#web_status.goal",
    scope_source,
    "prompts/thread_reply_redline.md",
    "prompts/thread_reply_format.md",
  ];
  assert_eq!(sources.len(), expected_sources.len(), "{platform} thread manager prompt sources: {sources:?}");
  for expected in expected_sources {
    assert!(
      sources.iter().any(|source| source.as_str() == expected),
      "{platform} thread manager prompt lacks {expected}"
    );
  }
}

#[test]
#[ignore = "integration"]
fn slack_thread_manager_loads_the_shared_core_and_platform_supplements() {
  assert_thread_manager_sources("Slack", "prompts/slack_reply_scope.md", &slack_thread_session());
}

#[test]
#[ignore = "integration"]
fn discord_thread_manager_loads_the_shared_core_and_platform_supplements() {
  assert_thread_manager_sources("Discord", "prompts/discord_reply_scope.md", &discord_thread_session());
}

#[test]
#[ignore = "integration"]
fn feishu_thread_manager_loads_the_shared_core_and_platform_supplements() {
  assert_thread_manager_sources("Feishu", "prompts/feishu_reply_scope.md", &feishu_thread_session());
}
