//! The web-reply package's turn contribution. It carries the reply
//! rules in a manager's runs, owns the web turn-end checks and notice delivery, renders the `web_reply`,
//! `web_reply_notice` and `web_status` events as chat lines, and claims turn-end unread state for manager sessions.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use charliebot_core::RepoRoot;
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::event_types::{
  COLD_READ_CHECK, WEB_ASK_CHECK, WEB_REPLY, WEB_REPLY_CHECK, WEB_REPLY_NOTICE, WEB_STATUS,
};
use charliebot_core::run_kind::RunKind;
use charliebot_core::str_methods;
use charliebot_registry::FeatureContext;
use charliebot_registry::turn::{
  CheckResult, DELIVERY_FULL, EventRenderer, PromptSource, RuleSegment, SCOPE_BASE, TurnContribution, TurnInput,
};
use charliebot_store::metadata::{SessionMetadata, TaskProfile};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};

use crate::{audit, pending_wake, reply, status};

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
    if kind != RunKind::ManagerTurn.as_str() || !reply::is_manager_session(meta) {
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

  fn applies_to_turn(&self, meta: &SessionMetadata) -> bool {
    meta.profile == TaskProfile::Manager
  }

  fn show_turn_notice<'a>(
    &'a self,
    features: &'a Arc<FeatureContext>,
    session_id: &'a str,
    turn: &'a [Value],
    message: &'a str,
  ) -> BoxFuture<'a, anyhow::Result<bool>> {
    Box::pin(audit::show_notice(features, session_id, turn, message))
  }

  /// The line that opens each user message of a manager session in the turn prompt. The turn prompt joins a turn's inputs
  /// with blank lines, so this line alone marks where one user message ends and the next begins.
  fn input_preamble(&self, meta: &SessionMetadata, _input_event: &Value) -> Option<String> {
    reply::is_manager_session(meta).then(|| "[User message]".to_string())
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
      (COLD_READ_CHECK, cold_read_check_message),
      (WEB_ASK_CHECK, cold_read_check_message),
      (WEB_REPLY_CHECK, cold_read_check_message),
    ]
  }

  /// A clean turn of a manager session leaves it unread only when its web checks or notices wrote something a user must
  /// read. A turn that only replied in a thread leaves nothing unread.
  fn claims_turn_end_unread(&self, meta: &SessionMetadata) -> bool {
    meta.profile == TaskProfile::Manager
  }

  fn turn_end_check_names(&self) -> &'static [&'static str] {
    &[audit::WEB_REPLY_ENTRY, audit::WEB_STATUS_ENTRY, audit::STATUS_REREAD_ENTRY, pending_wake::PENDING_WAKE_ENTRY]
  }

  fn run_turn_end_check<'a>(
    &'a self,
    name: &'a str,
    features: &'a Arc<FeatureContext>,
    input: &'a TurnInput,
  ) -> BoxFuture<'a, anyhow::Result<CheckResult>> {
    Box::pin(async move {
      match name {
        audit::WEB_REPLY_ENTRY => audit::check_web_reply(input),
        audit::WEB_STATUS_ENTRY => Ok(audit::check_web_status(input)),
        audit::STATUS_REREAD_ENTRY => Ok(audit::check_status_reread(input)),
        pending_wake::PENDING_WAKE_ENTRY => pending_wake::check_pending_wake(features, input).await,
        other => anyhow::bail!("the web contribution answers no turn-end check named {other}"),
      }
    })
  }
}

/// A prompts doc read as UTF-8 text: universal newlines, then both-sided whitespace trimmed. A
/// missing or unreadable doc fails.
fn rules_text(path: &Path) -> anyhow::Result<String> {
  let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
  Ok(str_methods::strip(&str_methods::universal_newlines(&text)).to_string())
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

/// The chat line of a cold-read check: the check's name and its content, whether the record is the unified
/// `cold_read_check` or a legacy `web_ask_check` / `web_reply_check`.
fn cold_read_check_message(event: &Value) -> Value {
  let name = crate::cold_read_record::cold_read_record_name(event).unwrap_or_default();
  json!({"role": COLD_READ_CHECK, "name": name, "content": content(event)})
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
  fn every_manager_turn_gets_the_reply_rules_but_worker_turns_do_not() {
    charliebot_registry::thread_origin::register_origin_field("slack", "slack_origin");
    charliebot_registry::thread_origin::register_origin_field("discord", "discord_origin");
    charliebot_registry::thread_origin::register_origin_field("feishu", "feishu_origin");
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
    assert!(
      contribution
        .instruction_segments(&worker, "manager_turn", &synthetic_config("/synthetic/home"), &repo)
        .unwrap()
        .is_empty()
    );
    for meta in [&web, &slack_thread, &discord_thread, &feishu_thread, &null_origin] {
      let segments =
        contribution.instruction_segments(meta, "manager_turn", &synthetic_config("/synthetic/home"), &repo).unwrap();
      assert_eq!(segments.len(), 1);
    }
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
    charliebot_registry::thread_origin::register_origin_field("slack", "slack_origin");
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
  fn every_manager_session_claims_the_turn_end_unread_mark_and_applies_its_checks() {
    charliebot_registry::thread_origin::register_origin_field("slack", "slack_origin");
    let contribution = WebReplyTurnContribution;
    let manager = SessionMetadata::new("synthetic-session", "Session 1", TaskProfile::Manager);
    let worker = SessionMetadata::new("synthetic-worker", "Worker", TaskProfile::Worker);
    let mut thread = manager.clone();
    thread.extra.insert("slack_origin".to_string(), json!({"channel_id":"synthetic-channel"}));

    assert!(contribution.claims_turn_end_unread(&manager));
    assert!(contribution.claims_turn_end_unread(&thread));
    assert!(!contribution.claims_turn_end_unread(&worker));
    assert!(contribution.applies_to_turn(&manager));
    assert!(contribution.applies_to_turn(&thread));
    assert!(!contribution.applies_to_turn(&worker));
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

  /// The unified record and both legacy types render through one branch, which names the check and keeps the content.
  #[test]
  fn every_cold_read_check_renders_its_name_under_the_one_role() {
    for (event, name) in [
      (
        json!({"type": COLD_READ_CHECK, "name": "settle", "content": "Cold read of the kept Waiting on you items"}),
        "settle",
      ),
      (
        json!({"type": COLD_READ_CHECK, "name": "remaining", "content": "Cold read of the Remaining list"}),
        "remaining",
      ),
      (json!({"type": WEB_REPLY_CHECK, "content": "web reply check"}), "reply"),
      (json!({"type": WEB_ASK_CHECK, "content": "Cold read of a Waiting on you item"}), "ask"),
    ] {
      let message = cold_read_check_message(&event);
      assert_eq!(message["role"], COLD_READ_CHECK);
      assert_eq!(message["name"], name);
      assert_eq!(message["content"], event["content"]);
    }
  }
}
