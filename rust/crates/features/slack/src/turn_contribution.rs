//! The Slack package's turn-end check, thread reply contract, and `slack_reply` chat renderer.

use std::sync::Arc;

use charliebot_chat_threads::reply_event::reply_line;
use charliebot_chat_threads::thread_entry::{self, ThreadAdapter, thread_reply_contract, thread_reply_segment};
use charliebot_core::RepoRoot;
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::run_kind::RunKind;
use charliebot_registry::FeatureContext;
use charliebot_registry::turn::{EventRenderer, RuleSegment, TurnContribution};
use charliebot_store::metadata::SessionMetadata;
use futures_util::future::BoxFuture;
use serde_json::Value;

use crate::event_types::SLACK_REPLY;
use crate::metadata::slack_origin;
use crate::thread_contract::{SLACK, thread_contract_part};

/// The Slack turn contribution.
pub struct SlackTurnContribution;

impl TurnContribution for SlackTurnContribution {
  fn instruction_segments(
    &self,
    meta: &SessionMetadata,
    kind: &str,
    cfg: &CharlieBotConfig,
    repo: &RepoRoot,
  ) -> anyhow::Result<Vec<RuleSegment>> {
    if kind != RunKind::ManagerTurn.as_str() {
      return Ok(Vec::new());
    }
    let Some(origin) = slack_origin(meta)? else {
      return Ok(Vec::new());
    };
    let contract = thread_reply_contract(repo, &cfg.charliebot_home, &SLACK, &thread_contract_part(&origin))?;
    Ok(vec![thread_reply_segment(contract)])
  }

  fn event_renderers(&self) -> Vec<(&'static str, EventRenderer)> {
    vec![(SLACK_REPLY, slack_reply_message)]
  }

  fn turn_kind(&self, meta: &SessionMetadata) -> Option<&'static str> {
    thread_entry::thread_turn_kind(meta, &SLACK)
  }

  fn show_turn_notice<'a>(
    &'a self,
    features: &'a Arc<FeatureContext>,
    session_id: &'a str,
    turn: &'a [Value],
    message: &'a str,
  ) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move {
      let adapter: Arc<dyn ThreadAdapter> =
        Arc::new(crate::slack_listener::SlackThreadAdapter::lazy(Arc::clone(features)));
      thread_entry::show_turn_notice(&adapter, features, session_id, turn, message).await?;
      Ok(())
    })
  }

  fn after_failed_turn<'a>(
    &'a self,
    features: &'a Arc<FeatureContext>,
    session_id: &'a str,
    done_event: &'a Value,
  ) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move {
      let adapter: Arc<dyn ThreadAdapter> =
        Arc::new(crate::slack_listener::SlackThreadAdapter::lazy(Arc::clone(features)));
      thread_entry::audit_failed_turn(&adapter, features, session_id, done_event).await?;
      Ok(())
    })
  }
}

/// The chat line of a `slack_reply` event.
fn slack_reply_message(event: &Value) -> Value {
  reply_line(&SLACK, event)
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
  use crate::metadata::SlackOrigin;

  const ORIGIN_TS: &str = "1700000000.000100";

  /// A synthetic checkout whose three contract docs hold short marker texts, and which holds `docs` only.
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
      ("slack_reply_scope.md", "SCOPE"),
      ("thread_reply_redline.md", "RED"),
      ("thread_reply_format.md", "FORMAT"),
    ])
  }

  /// A manager session with the given `slack_origin` field value.
  fn manager_with(origin: Value) -> SessionMetadata {
    let mut meta = SessionMetadata::new("synthetic-session", "Session 1", TaskProfile::Manager);
    meta.extra.insert("slack_origin".to_string(), origin);
    meta
  }

  fn synthetic_origin() -> SlackOrigin {
    SlackOrigin { team_id: "T1".into(), channel_id: "C1".into(), thread_ts: ORIGIN_TS.into() }
  }

  #[test]
  fn a_manager_run_of_a_slack_thread_gets_the_contract_as_one_full_segment() {
    let (_dir, repo) = full_checkout();
    let origin = synthetic_origin();
    let meta = manager_with(json!({"team_id": "T1", "channel_id": "C1", "thread_ts": ORIGIN_TS}));
    let segments = SlackTurnContribution
      .instruction_segments(&meta, "manager_turn", &synthetic_config("/synthetic/home"), &repo)
      .unwrap();

    assert_eq!(segments.len(), 1);
    assert_eq!(segments[0].text, format!("{}\n\nSCOPE\nRED\nFORMAT", thread_contract_part(&origin)));
    assert_eq!(segments[0].delivery, "full");
    let refs: Vec<&str> = segments[0].sources.iter().map(|source| source.source_ref.as_str()).collect();
    assert_eq!(
      refs,
      ["prompts/slack_reply_scope.md", "prompts/thread_reply_redline.md", "prompts/thread_reply_format.md"]
    );
    assert!(segments[0].sources.iter().all(|source| source.scope == "base" && source.source_session_id.is_none()));
  }

  #[test]
  fn contract_part_carries_the_platform_line_the_address_and_the_rules() {
    let part = thread_contract_part(&synthetic_origin());
    assert!(part.contains("C1") && part.contains(ORIGIN_TS), "{part}");
  }

  #[test]
  fn only_a_manager_run_of_a_slack_thread_gets_the_contract() {
    let (_dir, repo) = full_checkout();
    let origin = json!({"team_id": "T1", "channel_id": "C1", "thread_ts": ORIGIN_TS});
    let contribution = SlackTurnContribution;
    for kind in ["work", "review"] {
      let segments = contribution.instruction_segments(
        &manager_with(origin.clone()),
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
    let meta = manager_with(json!({"team_id": "T1", "channel_id": "C1"}));
    let error = SlackTurnContribution
      .instruction_segments(&meta, "manager_turn", &synthetic_config("/synthetic/home"), &repo)
      .unwrap_err();
    assert!(error.to_string().starts_with("slack_origin is not a Slack origin"), "{error}");
  }

  #[test]
  fn a_missing_contract_doc_fails_the_build_with_its_name_and_cause() {
    let (_dir, repo) = checkout(&[("slack_reply_scope.md", "SCOPE"), ("thread_reply_format.md", "FORMAT")]);
    let meta = manager_with(json!({"team_id": "T1", "channel_id": "C1", "thread_ts": ORIGIN_TS}));
    let error = SlackTurnContribution
      .instruction_segments(&meta, "manager_turn", &synthetic_config("/synthetic/home"), &repo)
      .unwrap_err();
    assert!(error.to_string().starts_with("thread_reply_redline.md prompt not found at "), "{error}");
  }

  #[test]
  fn slack_reply_renders_its_content() {
    let event = json!({"content": "Done"});
    assert_eq!(slack_reply_message(&event).to_string(), r#"{"role":"system","content":"Posted to Slack: Done"}"#);
  }

  #[test]
  fn slack_reply_without_content_renders_an_empty_content() {
    let event = json!({});
    assert_eq!(slack_reply_message(&event).to_string(), r#"{"role":"system","content":"Posted to Slack: "}"#);
  }

  #[test]
  fn slack_reply_uses_string_conversion_for_non_string_content() {
    let event = json!({"content": {"message": "Done", "sent": true}});
    assert_eq!(
      slack_reply_message(&event).to_string(),
      r#"{"role":"system","content":"Posted to Slack: {'message': 'Done', 'sent': True}"}"#
    );
  }
}
