//! The web-reply package's turn contribution. It carries the reply
//! rules in a web session's manager runs, owns the web turn-end checks and notice delivery, renders the
//! `web_reply`, `web_reply_notice` and `web_status` events as chat lines, and claims turn-end unread state for web
//! sessions.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use charliebot_core::RepoRoot;
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::event_types::{WEB_ASK_CHECK, WEB_REPLY, WEB_REPLY_CHECK, WEB_REPLY_NOTICE, WEB_STATUS};
use charliebot_core::run_kind::RunKind;
use charliebot_core::str_methods;
use charliebot_registry::FeatureContext;
use charliebot_registry::turn::{
  DELIVERY_FULL, EventRenderer, PromptSource, RuleSegment, SCOPE_BASE, TurnContribution,
};
use charliebot_store::metadata::{SessionMetadata, TaskProfile};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};

use crate::{audit, reply, status};

/// The web-reply turn contribution.
pub struct WebReplyTurnContribution;

impl TurnContribution for WebReplyTurnContribution {
  fn instruction_segments(
    &self,
    meta: &SessionMetadata,
    kind: &str,
    _cfg: &CharlieBotConfig,
    repo: &RepoRoot,
  ) -> anyhow::Result<Vec<RuleSegment>> {
    if kind != RunKind::ManagerTurn.as_str() || !reply::is_web_session(meta) {
      return Ok(Vec::new());
    }
    let text = rules_text(&repo.path().join("prompts").join(reply::RULES_FILE))?;
    let source = PromptSource {
      scope: SCOPE_BASE.to_string(),
      source_ref: format!("prompts/{}", reply::RULES_FILE),
      source_session_id: None,
    };
    Ok(vec![RuleSegment { text, sources: vec![source], delivery: DELIVERY_FULL.to_string() }])
  }

  fn turn_kind(&self, meta: &SessionMetadata) -> Option<&'static str> {
    reply::is_web_session(meta).then_some("web")
  }

  fn show_turn_notice<'a>(
    &'a self,
    features: &'a Arc<FeatureContext>,
    session_id: &'a str,
    turn: &'a [Value],
    message: &'a str,
  ) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(audit::show_notice(features, session_id, turn, message))
  }

  /// The line that opens each user message of a web session in the turn prompt. The turn prompt joins a turn's inputs
  /// with blank lines, so this line alone marks where one user message ends and the next begins.
  fn input_preamble(&self, meta: &SessionMetadata, _input_event: &Value) -> Option<String> {
    reply::is_web_session(meta).then(|| "[User message]".to_string())
  }

  /// The status Goal of a manager session, web or thread. A worker stores no status.
  fn status_goal(&self, meta: &SessionMetadata) -> anyhow::Result<Option<String>> {
    if meta.profile != TaskProfile::Manager {
      return Ok(None);
    }
    status::stored_goal(meta)
  }

  fn event_renderers(&self) -> Vec<(&'static str, EventRenderer)> {
    vec![
      (WEB_REPLY, web_reply_message),
      (WEB_REPLY_NOTICE, web_reply_notice_message),
      (WEB_STATUS, web_status_message),
      (WEB_ASK_CHECK, web_ask_check_message),
      (WEB_REPLY_CHECK, web_reply_check_message),
    ]
  }

  fn claims_turn_end_unread(&self, meta: &SessionMetadata) -> bool {
    reply::is_web_session(meta)
  }
}

/// A prompts doc read as UTF-8 text: universal newlines, then both-sided whitespace trimmed. A
/// missing or unreadable doc fails.
fn rules_text(path: &Path) -> anyhow::Result<String> {
  let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
  Ok(str_methods::strip(&text.replace("\r\n", "\n").replace('\r', "\n")).to_string())
}

/// The chat line of a `web_reply` event.
fn web_reply_message(event: &Value) -> Value {
  json!({"role": WEB_REPLY, "content": content(event)})
}

/// The chat line of a `web_reply_notice` event.
fn web_reply_notice_message(event: &Value) -> Value {
  json!({"role": WEB_REPLY_NOTICE, "content": content(event)})
}

/// The chat line of a `web_status` event, with the event's status value, or `null`.
fn web_status_message(event: &Value) -> Value {
  json!({"role": WEB_STATUS, "content": content(event), (WEB_STATUS): event.get(WEB_STATUS)})
}

/// The chat line of a `web_ask_check` event.
fn web_ask_check_message(event: &Value) -> Value {
  json!({"role": WEB_ASK_CHECK, "content": content(event)})
}

/// The chat line of a `web_reply_check` event.
fn web_reply_check_message(event: &Value) -> Value {
  json!({"role": WEB_REPLY_CHECK, "content": content(event)})
}

/// The event's `content`, or `""` when the event has none (`event.get("content", "")`).
fn content(event: &Value) -> Value {
  event.get("content").cloned().unwrap_or_else(|| json!(""))
}

#[cfg(test)]
mod tests {
  use std::ffi::OsString;

  use charliebot_core::test_support::synthetic_config;
  use charliebot_store::metadata::{SessionMetadata, TaskProfile};
  use serde_json::json;
  use tempfile::TempDir;

  use super::*;

  /// A synthetic checkout whose `prompts/web_reply.md` holds `text`.
  fn checkout(text: &str) -> (TempDir, RepoRoot) {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("prompts")).unwrap();
    fs::write(dir.path().join("prompts").join(reply::RULES_FILE), text).unwrap();
    let repo = RepoRoot::new(Some(OsString::from(dir.path()))).unwrap();
    (dir, repo)
  }

  fn manager(extra: Value) -> SessionMetadata {
    let mut meta = SessionMetadata::new("synthetic-session", "Session 1", TaskProfile::Manager);
    if let Value::Object(fields) = extra {
      meta.extra.extend(fields);
    }
    meta
  }

  #[test]
  fn a_web_manager_run_gets_the_reply_rules_as_one_full_segment() {
    let (_dir, repo) = checkout("\n  Reply in the chat.\r\nSecond line.\n\n");
    let segments = WebReplyTurnContribution
      .instruction_segments(&manager(json!({})), "manager_turn", &synthetic_config("/synthetic/home"), &repo)
      .unwrap();

    assert_eq!(segments.len(), 1);
    assert_eq!(segments[0].text, "Reply in the chat.\nSecond line.");
    assert_eq!(segments[0].delivery, "full");
    assert_eq!(
      segments[0].sources,
      [PromptSource {
        scope: "base".to_string(),
        source_ref: "prompts/web_reply.md".to_string(),
        source_session_id: None,
      }]
    );
  }

  #[test]
  fn only_a_manager_run_of_a_web_session_gets_the_rules() {
    charliebot_registry::thread_origin::register_origin_field("slack_origin");
    charliebot_registry::thread_origin::register_origin_field("discord_origin");
    charliebot_registry::thread_origin::register_origin_field("feishu_origin");
    let (_dir, repo) = checkout("rules");
    let web = manager(json!({}));
    let worker = SessionMetadata::new("synthetic-worker", "Worker", TaskProfile::Worker);
    let slack_thread = manager(json!({"slack_origin": {"channel_id": "synthetic-channel"}}));
    let discord_thread = manager(json!({"discord_origin": {"thread_id": "synthetic-thread"}}));
    let feishu_thread = manager(json!({"feishu_origin": {"tenant_key": "T", "chat_id": "oc_1", "root_id": "om_1"}}));
    let null_origin = manager(json!({"slack_origin": null, "discord_origin": null, "feishu_origin": null}));

    let contribution = WebReplyTurnContribution;
    for kind in ["work", "review"] {
      assert!(
        contribution.instruction_segments(&web, kind, &synthetic_config("/synthetic/home"), &repo).unwrap().is_empty()
      );
    }
    for meta in [&worker, &slack_thread, &discord_thread, &feishu_thread] {
      assert!(
        contribution
          .instruction_segments(meta, "manager_turn", &synthetic_config("/synthetic/home"), &repo)
          .unwrap()
          .is_empty()
      );
    }
    assert_eq!(
      contribution
        .instruction_segments(&null_origin, "manager_turn", &synthetic_config("/synthetic/home"), &repo)
        .unwrap()
        .len(),
      1
    );
  }

  #[test]
  fn a_web_manager_run_fails_when_the_rules_doc_is_missing() {
    let (dir, repo) = checkout("rules");
    fs::remove_file(dir.path().join("prompts").join(reply::RULES_FILE)).unwrap();

    let error = WebReplyTurnContribution
      .instruction_segments(&manager(json!({})), "manager_turn", &synthetic_config("/synthetic/home"), &repo)
      .unwrap_err();
    assert!(error.to_string().starts_with("read "), "{error}");
  }

  /// The status Goal hook answers for a manager session, web or thread, that stores a Goal. A worker, or a manager
  /// without a stored Goal, has none to give.
  #[test]
  fn the_status_goal_is_the_stored_goal_of_any_manager_session() {
    charliebot_registry::thread_origin::register_origin_field("slack_origin");
    let contribution = WebReplyTurnContribution;
    let with_goal = manager(json!({"web_status": {"goal": "ship the release", "waiting": []}}));
    let without_goal = manager(json!({"web_status": {"now": "building", "waiting": []}}));
    let worker = SessionMetadata::new("synthetic-worker", "Worker", TaskProfile::Worker);
    let slack_thread = manager(json!({
      "slack_origin": {"channel_id": "synthetic-channel"},
      "web_status": {"goal": "ship the release", "waiting": []},
    }));

    assert_eq!(contribution.status_goal(&with_goal).unwrap(), Some("ship the release".to_string()));
    assert_eq!(contribution.status_goal(&manager(json!({}))).unwrap(), None);
    assert_eq!(contribution.status_goal(&without_goal).unwrap(), None);
    assert_eq!(contribution.status_goal(&worker).unwrap(), None);
    assert_eq!(contribution.status_goal(&slack_thread).unwrap(), Some("ship the release".to_string()));
  }

  /// An unreadable stored status fails the hook instead of silently leaving the Goal out.
  #[test]
  fn an_unreadable_status_fails_the_status_goal() {
    let contribution = WebReplyTurnContribution;
    let unreadable = manager(json!({"web_status": {"goal": 7, "waiting": []}}));

    let error = contribution.status_goal(&unreadable).unwrap_err();

    assert!(error.to_string().starts_with("session synthetic-session holds an unreadable web status"), "{error}");
  }

  #[test]
  fn only_plain_manager_sessions_claim_the_turn_end_unread_mark() {
    charliebot_registry::thread_origin::register_origin_field("slack_origin");
    let contribution = WebReplyTurnContribution;
    let manager = SessionMetadata::new("synthetic-session", "Session 1", TaskProfile::Manager);
    let worker = SessionMetadata::new("synthetic-worker", "Worker", TaskProfile::Worker);
    let mut thread = manager.clone();
    thread.extra.insert("slack_origin".to_string(), json!({"channel_id":"synthetic-channel"}));

    assert!(contribution.claims_turn_end_unread(&manager));
    assert!(!contribution.claims_turn_end_unread(&worker));
    assert!(!contribution.claims_turn_end_unread(&thread));
  }

  #[test]
  fn web_reply_renders_its_content() {
    let event = json!({"content": "Hi"});
    assert_eq!(web_reply_message(&event).to_string(), r#"{"role":"web_reply","content":"Hi"}"#);
  }

  #[test]
  fn web_reply_without_content_renders_an_empty_content() {
    let event = json!({});
    assert_eq!(web_reply_message(&event).to_string(), r#"{"role":"web_reply","content":""}"#);
  }

  #[test]
  fn web_reply_notice_renders_its_content() {
    let event = json!({"content": "Queued"});
    assert_eq!(web_reply_notice_message(&event).to_string(), r#"{"role":"web_reply_notice","content":"Queued"}"#);
  }

  #[test]
  fn web_reply_notice_without_content_renders_an_empty_content() {
    let event = json!({});
    assert_eq!(web_reply_notice_message(&event).to_string(), r#"{"role":"web_reply_notice","content":""}"#);
  }

  #[test]
  fn web_status_renders_its_content_and_status() {
    let event = json!({"content": "Working", "web_status": "running"});
    assert_eq!(
      web_status_message(&event).to_string(),
      r#"{"role":"web_status","content":"Working","web_status":"running"}"#
    );
  }

  #[test]
  fn web_status_without_content_or_status_renders_empty_and_null() {
    let event = json!({});
    assert_eq!(web_status_message(&event).to_string(), r#"{"role":"web_status","content":"","web_status":null}"#);
  }
}
