//! The Discord package's turn-end check, thread reply contract, and `discord_reply` chat renderer.

use std::sync::Arc;

use charliebot_chat_threads::reply_event::reply_line;
use charliebot_chat_threads::thread_entry::{self, thread_reply_contract, thread_reply_segment};
use charliebot_core::RepoRoot;
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::run_kind::RunKind;
use charliebot_registry::FeatureContext;
use charliebot_registry::turn::{EventRenderer, RuleSegment, TurnContribution};
use charliebot_store::metadata::SessionMetadata;
use futures_util::future::BoxFuture;
use serde_json::Value;

use crate::discord_listener::{self, DISCORD};
use crate::event_types::DISCORD_REPLY;
use crate::metadata::discord_origin;
use crate::thread_contract::thread_contract_part;

/// The Discord turn contribution.
pub struct DiscordTurnContribution;

impl TurnContribution for DiscordTurnContribution {
  fn instruction_segments(
    &self,
    meta: &SessionMetadata,
    kind: &str,
    cfg: &CharlieBotConfig,
    repo: &RepoRoot,
  ) -> anyhow::Result<Vec<RuleSegment>> {
    if kind != RunKind::ManagerTurn.as_str() || discord_origin(meta)?.is_none() {
      return Ok(Vec::new());
    }
    let contract = thread_reply_contract(repo, &cfg.charliebot_home, &DISCORD, &thread_contract_part())?;
    Ok(vec![thread_reply_segment(contract)])
  }

  fn event_renderers(&self) -> Vec<(&'static str, EventRenderer)> {
    vec![(DISCORD_REPLY, discord_reply_message)]
  }

  fn turn_kind(&self, meta: &SessionMetadata) -> Option<&'static str> {
    thread_entry::thread_turn_kind(meta, &DISCORD)
  }

  fn turn_check_sender(&self, _entry: &str) -> &'static str {
    DISCORD.display_name
  }

  fn show_turn_notice<'a>(
    &'a self,
    features: &'a Arc<FeatureContext>,
    session_id: &'a str,
    turn: &'a [Value],
    message: &'a str,
  ) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(discord_listener::show_turn_notice(features, session_id, turn, message))
  }

  fn after_failed_turn<'a>(
    &'a self,
    features: &'a Arc<FeatureContext>,
    session_id: &'a str,
    done_event: &'a Value,
  ) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(discord_listener::after_failed_turn(features, session_id, done_event))
  }
}

/// The chat line of a `discord_reply` event.
fn discord_reply_message(event: &Value) -> Value {
  reply_line(&DISCORD, event)
}

#[cfg(test)]
mod tests {
  use std::ffi::OsString;
  use std::fs;

  use charliebot_core::test_support::synthetic_config;
  use charliebot_store::metadata::{SessionMetadata, TaskProfile};
  use serde_json::json;
  use tempfile::TempDir;

  use super::*;

  /// A synthetic checkout whose contract docs hold short marker texts, and which holds `docs` only.
  fn checkout(docs: &[(&str, &str)]) -> (TempDir, RepoRoot) {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("prompts")).unwrap();
    for (name, text) in docs {
      fs::write(dir.path().join("prompts").join(name), text).unwrap();
    }
    let repo = RepoRoot::new(Some(OsString::from(dir.path()))).unwrap();
    (dir, repo)
  }

  fn full_checkout() -> (TempDir, RepoRoot) {
    checkout(&[
      ("discord_reply_scope.md", "SCOPE"),
      ("thread_reply_redline.md", "RED"),
      ("thread_reply_format.md", "FORMAT"),
    ])
  }

  /// A manager session with the given `discord_origin` field value.
  fn manager_with(origin: Value) -> SessionMetadata {
    let mut meta = SessionMetadata::new("synthetic-session", "Session 1", TaskProfile::Manager);
    meta.extra.insert("discord_origin".to_string(), origin);
    meta
  }

  fn synthetic_origin() -> Value {
    json!({"guild_id": "G1", "parent_channel_id": "C1", "thread_id": "1234567890"})
  }

  #[test]
  fn a_manager_run_of_a_discord_thread_gets_the_contract_as_one_full_segment() {
    let (_dir, repo) = full_checkout();
    let segments = DiscordTurnContribution
      .instruction_segments(
        &manager_with(synthetic_origin()),
        "manager_turn",
        &synthetic_config("/synthetic/home"),
        &repo,
      )
      .unwrap();

    assert_eq!(segments.len(), 1);
    assert_eq!(segments[0].text, format!("{}\n\nSCOPE\nRED\nFORMAT", thread_contract_part()));
    assert_eq!(segments[0].delivery, "full");
    let refs: Vec<&str> = segments[0].sources.iter().map(|source| source.source_ref.as_str()).collect();
    assert_eq!(
      refs,
      ["prompts/discord_reply_scope.md", "prompts/thread_reply_redline.md", "prompts/thread_reply_format.md"]
    );
    assert!(segments[0].sources.iter().all(|source| source.scope == "base" && source.source_session_id.is_none()));
  }

  #[test]
  fn only_a_manager_run_of_a_discord_thread_gets_the_contract() {
    let (_dir, repo) = full_checkout();
    let contribution = DiscordTurnContribution;
    for kind in ["work", "review"] {
      let segments = contribution.instruction_segments(
        &manager_with(synthetic_origin()),
        kind,
        &synthetic_config("/synthetic/home"),
        &repo,
      );
      assert!(segments.unwrap().is_empty(), "{kind}");
    }
    let no_origin = SessionMetadata::new("synthetic-session", "Session 1", TaskProfile::Manager);
    assert!(
      contribution
        .instruction_segments(&no_origin, "manager_turn", &synthetic_config("/synthetic/home"), &repo)
        .unwrap()
        .is_empty()
    );
    let null_origin = manager_with(Value::Null);
    assert!(
      contribution
        .instruction_segments(&null_origin, "manager_turn", &synthetic_config("/synthetic/home"), &repo)
        .unwrap()
        .is_empty()
    );
  }

  #[test]
  fn a_stored_origin_missing_a_field_fails_the_build() {
    let (_dir, repo) = full_checkout();
    let meta = manager_with(json!({"guild_id": "G1", "parent_channel_id": "C1"}));
    let error = DiscordTurnContribution
      .instruction_segments(&meta, "manager_turn", &synthetic_config("/synthetic/home"), &repo)
      .unwrap_err();
    assert!(error.to_string().starts_with("discord_origin is not a Discord origin"), "{error}");
  }

  #[test]
  fn a_missing_contract_doc_fails_the_build_with_its_name() {
    let (_dir, repo) = checkout(&[("thread_reply_redline.md", "RED"), ("thread_reply_format.md", "FORMAT")]);
    let error = DiscordTurnContribution
      .instruction_segments(
        &manager_with(synthetic_origin()),
        "manager_turn",
        &synthetic_config("/synthetic/home"),
        &repo,
      )
      .unwrap_err();
    assert!(error.to_string().starts_with("discord_reply_scope.md prompt not found at "), "{error}");
  }

  #[test]
  fn discord_reply_renders_its_content() {
    let event = json!({"content": "Done"});
    assert_eq!(discord_reply_message(&event).to_string(), r#"{"role":"system","content":"Posted to Discord: Done"}"#);
  }

  #[test]
  fn discord_reply_without_content_renders_an_empty_content() {
    let event = json!({});
    assert_eq!(discord_reply_message(&event).to_string(), r#"{"role":"system","content":"Posted to Discord: "}"#);
  }

  #[test]
  fn discord_reply_uses_string_conversion_for_non_string_content() {
    let event = json!({"content": {"message": "Done", "sent": true}});
    assert_eq!(
      discord_reply_message(&event).to_string(),
      r#"{"role":"system","content":"Posted to Discord: {'message': 'Done', 'sent': True}"}"#
    );
  }
}
