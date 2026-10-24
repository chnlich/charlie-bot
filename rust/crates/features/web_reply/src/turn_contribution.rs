//! The web-reply package's turn contribution (`src/features/web_reply/turn_contribution.py`). It carries the reply
//! rules in a web session's manager runs, runs the round-end audit after a turn that answers a user, renders the
//! `web_reply`, `web_reply_notice` and `web_status` events as chat lines, and claims turn-end unread state for web
//! sessions.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use charliebot_core::RepoRoot;
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::event_types::{WEB_ASK_CHECK, WEB_REPLY, WEB_REPLY_NOTICE, WEB_STATUS};
use charliebot_core::pystr;
use charliebot_registry::FeatureContext;
use charliebot_registry::turn::{EventRenderer, PromptSource, RuleSegment, TurnContribution};
use charliebot_store::metadata::SessionMetadata;
use futures_util::future::BoxFuture;
use serde_json::{Value, json};

use crate::{audit, reply};

/// The run kind that takes the manager contract (`task_prompts.MANAGER_KINDS`).
const MANAGER_KIND: &str = "manager_turn";
/// The scope of a rule segment's sources (`task_prompts.SCOPE_BASE`).
const SCOPE_BASE: &str = "base";
/// The delivery of a full text segment (`task_prompts.DELIVERY_FULL`).
const DELIVERY_FULL: &str = "full";

/// The web-reply turn contribution.
pub struct WebReplyTurnContribution;

impl TurnContribution for WebReplyTurnContribution {
  fn class_name(&self) -> &'static str {
    "WebReplyTurnContribution"
  }

  fn instruction_segments(
    &self,
    meta: &SessionMetadata,
    kind: &str,
    _cfg: &CharlieBotConfig,
    repo: &RepoRoot,
  ) -> anyhow::Result<Vec<RuleSegment>> {
    if kind != MANAGER_KIND || !reply::is_web_session(meta) {
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

  fn after_turn<'a>(
    &'a self,
    meta: &'a SessionMetadata,
    done_event: &'a Value,
    _cfg: &'a CharlieBotConfig,
    features: &'a Arc<FeatureContext>,
  ) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move {
      if !reply::is_web_session(meta) {
        return Ok(());
      }
      // An errored turn is not audited: its error event already shows and marks the session unread.
      let exit_code = done_event.get("exit_code").context("the master_done event has no exit_code")?;
      if exit_code.as_i64() != Some(0) {
        return Ok(());
      }
      let archived = meta.status == charliebot_store::metadata::SessionStatus::Archived;
      audit::audit_turn(features, &meta.id, done_event, archived).await
    })
  }

  fn event_renderers(&self) -> Vec<(&'static str, EventRenderer)> {
    vec![
      (WEB_REPLY, web_reply_message),
      (WEB_REPLY_NOTICE, web_reply_notice_message),
      (WEB_STATUS, web_status_message),
      (WEB_ASK_CHECK, web_ask_check_message),
    ]
  }

  fn claims_turn_end_unread(&self, meta: &SessionMetadata) -> bool {
    reply::is_web_session(meta)
  }
}

/// A prompts doc read as `read_text(encoding="utf-8").strip()` reads it: universal newlines, then Python's strip. A
/// missing or unreadable doc fails.
fn rules_text(path: &Path) -> anyhow::Result<String> {
  let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
  Ok(pystr::strip(&text.replace("\r\n", "\n").replace('\r', "\n")).to_string())
}

/// `_web_reply_message`: the chat line of a `web_reply` event.
fn web_reply_message(event: &Value) -> Value {
  json!({"role": WEB_REPLY, "content": content(event)})
}

/// `_web_reply_notice_message`: the chat line of a `web_reply_notice` event.
fn web_reply_notice_message(event: &Value) -> Value {
  json!({"role": WEB_REPLY_NOTICE, "content": content(event)})
}

/// `_web_status_message`: the chat line of a `web_status` event, with the event's status value, or `null`.
fn web_status_message(event: &Value) -> Value {
  json!({"role": WEB_STATUS, "content": content(event), "web_status": event.get(WEB_STATUS)})
}

/// `_web_ask_check_message`: the chat line of a `web_ask_check` event.
fn web_ask_check_message(event: &Value) -> Value {
  json!({"role": WEB_ASK_CHECK, "content": content(event)})
}

/// The event's `content`, or `""` when the event has none (`event.get("content", "")`).
fn content(event: &Value) -> Value {
  event.get("content").cloned().unwrap_or_else(|| json!(""))
}

#[cfg(test)]
mod tests {
  use std::ffi::OsString;

  use charliebot_core::config::{
    BackendsConfig, CharlieBotConfig, PathsConfig, Sections, ServerConfig, TelegramConfig, UiConfig,
  };
  use charliebot_store::metadata::{SessionMetadata, TaskProfile};
  use serde_json::json;
  use tempfile::TempDir;

  use super::*;
  use crate::test_support::{Home, id_of, master_done, user_event};

  /// A synthetic checkout whose `prompts/web_reply.md` holds `text`.
  fn checkout(text: &str) -> (TempDir, RepoRoot) {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("prompts")).unwrap();
    fs::write(dir.path().join("prompts").join(reply::RULES_FILE), text).unwrap();
    let repo = RepoRoot::new(Some(OsString::from(dir.path()))).unwrap();
    (dir, repo)
  }

  fn config() -> CharlieBotConfig {
    CharlieBotConfig {
      charliebot_home: "/synthetic/home".into(),
      headless_chrome_bin: "/synthetic/bin/chrome".to_string(),
      server: ServerConfig {
        host: "127.0.0.1".to_string(),
        port: 18650,
        subprocess_buffer_limit_mb: 1,
        session_memory_max_mb: 0,
        session_swap_max_mb: 0,
        session_tasks_max: 0,
        min_free_disk_gib: 0,
      },
      paths: PathsConfig {
        workspace_dirs: Vec::new(),
        worktree_dir: "/synthetic/worktrees".to_string(),
        scratch_dir: "/synthetic/scripts".to_string(),
      },
      backends: BackendsConfig { pool: Vec::new(), routing: None, options: Vec::new() },
      ui: UiConfig { home_services: Vec::new() },
      telegram: TelegramConfig { chat_id: None },
      sections: Sections::default(),
    }
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
    let segments =
      WebReplyTurnContribution.instruction_segments(&manager(json!({})), "manager_turn", &config(), &repo).unwrap();

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
    let (_dir, repo) = checkout("rules");
    let web = manager(json!({}));
    let worker = SessionMetadata::new("synthetic-worker", "Worker", TaskProfile::Worker);
    let slack_thread = manager(json!({"slack_origin": {"channel_id": "synthetic-channel"}}));
    let discord_thread = manager(json!({"discord_origin": {"thread_id": "synthetic-thread"}}));
    let null_origin = manager(json!({"slack_origin": null, "discord_origin": null}));

    let contribution = WebReplyTurnContribution;
    for kind in ["work", "review"] {
      assert!(contribution.instruction_segments(&web, kind, &config(), &repo).unwrap().is_empty());
    }
    for meta in [&worker, &slack_thread, &discord_thread] {
      assert!(contribution.instruction_segments(meta, "manager_turn", &config(), &repo).unwrap().is_empty());
    }
    assert_eq!(contribution.instruction_segments(&null_origin, "manager_turn", &config(), &repo).unwrap().len(), 1);
  }

  #[test]
  fn a_web_manager_run_fails_when_the_rules_doc_is_missing() {
    let (dir, repo) = checkout("rules");
    fs::remove_file(dir.path().join("prompts").join(reply::RULES_FILE)).unwrap();

    let error =
      WebReplyTurnContribution.instruction_segments(&manager(json!({})), "manager_turn", &config(), &repo).unwrap_err();
    assert!(error.to_string().starts_with("read "), "{error}");
  }

  #[test]
  fn only_plain_manager_sessions_claim_the_turn_end_unread_mark() {
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

  #[tokio::test]
  async fn after_turn_audits_only_a_web_turn_that_answers_a_user() {
    for (session_kind, input_kind, expect_nudge) in
      [("web", "user", true), ("web", "background", false), ("thread", "user", false)]
    {
      let home = Home::new();
      let origin = match session_kind {
        "web" => None,
        "thread" => {
          Some(("slack_origin", json!({"team_id": "T1", "channel_id": "C1", "thread_ts": "1700000000.000100"})))
        }
        other => unreachable!("no session kind {other}"),
      };
      let sid = home.manager("session", origin).await;
      let input = match input_kind {
        "user" => user_event("the question"),
        "background" => json!({"type": "scheduled_trigger", "content": "wake"}),
        other => unreachable!("no input kind {other}"),
      };
      let input_id = id_of(&home.append(&sid, input).await).to_string();
      let meta = home.meta(&sid).await;

      WebReplyTurnContribution
        .after_turn(&meta, &master_done(&[input_id.as_str()], 0), &config(), &home.features)
        .await
        .unwrap();

      assert_eq!(!home.nudges(&sid).is_empty(), expect_nudge, "{session_kind} session, {input_kind} input");
    }
  }

  // -------------------------------------------------------------------------
  // The status reminder: every finished turn, once, and no reminder from its own wake
  // -------------------------------------------------------------------------

  /// The web contribution's after_turn for one finished turn, over the test home's feature context.
  async fn after_turn(home: &Home, session_id: &str, done: &Value) {
    let meta = home.meta(session_id).await;
    WebReplyTurnContribution.after_turn(&meta, done, &config(), &home.features).await.unwrap();
  }

  /// The status reminders of the session's log, in log order: the agent messages a turn's missing status set or
  /// confirm woke (`_status_reminders`).
  fn status_reminders(home: &Home, session_id: &str) -> Vec<Value> {
    home.events(session_id).into_iter().filter(|event| event.get("web_status_reminder").is_some()).collect()
  }

  /// A master_done event that answers `input_event_ids`, carries `id` as its persisted id, and ended cleanly.
  fn done_with_id(input_event_ids: &[&str], id: &str) -> Value {
    json!({"type": "master_done", "exit_code": 0, "input_event_ids": input_event_ids, "id": id})
  }

  #[tokio::test]
  async fn a_turn_without_a_status_gets_one_reminder_and_its_reminder_turn_gets_none() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let done = done_with_id(&[], "turn-1");

    after_turn(&home, &sid, &done).await;
    after_turn(&home, &sid, &done).await;

    let reminders = status_reminders(&home, &sid);
    assert_eq!(reminders.len(), 1);
    assert_eq!(reminders[0]["type"], json!("agent_message"));
    assert_eq!(reminders[0]["web_status_reminder"], json!("turn-1"));
    assert_eq!(reminders[0]["from_session_name"], json!("Session status"));
    assert_eq!(reminders[0].get("web_reply"), None, "a plain reminder answers no user message");

    let reminder_id = id_of(&reminders[0]).to_string();
    after_turn(&home, &sid, &done_with_id(&[reminder_id.as_str()], "turn-2")).await;

    assert_eq!(status_reminders(&home, &sid).len(), 1, "the reminder's own turn gets no reminder");
  }

  #[tokio::test]
  async fn a_turn_missing_reply_and_status_gets_one_combined_reminder_and_one_wake() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let user = home.append(&sid, user_event("the question")).await;
    let done = done_with_id(&[id_of(&user)], "turn-1");

    after_turn(&home, &sid, &done).await;

    let events = home.events(&sid);
    let reminders: Vec<Value> =
      events.iter().filter(|event| event.get("web_status_reminder").is_some()).cloned().collect();
    assert_eq!(reminders.len(), 1);
    assert_eq!(reminders[0]["web_reply"]["nudge_of"], json!(id_of(&user)));
    // The combined reminder is the turn's one nudge: no separate reply nudge rides it.
    let nudges = home.nudges(&sid);
    assert_eq!(nudges.len(), 1);
    assert_eq!(id_of(&nudges[0]), id_of(&reminders[0]));
    assert_eq!(events.iter().filter(|event| event["type"] == "agent_message").count(), 1);

    let reminder_id = id_of(&reminders[0]).to_string();
    after_turn(&home, &sid, &done_with_id(&[reminder_id.as_str()], "turn-2")).await;

    let events = home.events(&sid);
    assert_eq!(events.iter().filter(|event| event.get("web_status_reminder").is_some()).count(), 1);
    assert_eq!(events.iter().filter(|event| event["type"] == "web_reply_notice").count(), 1);
  }

  #[tokio::test]
  async fn a_pending_close_request_skips_only_the_status_reminder() {
    for missing_reply in [false, true] {
      let home = Home::new();
      let sid = home.manager("session", None).await;
      let mut input_ids: Vec<String> = Vec::new();
      if missing_reply {
        let user = home.append(&sid, user_event("the question")).await;
        input_ids.push(id_of(&user).to_string());
      }
      home
        .append(&sid, json!({"type": "task_close_requested", "request_id": "close-1", "owner_run_id": "run-1"}))
        .await;
      let input_refs: Vec<&str> = input_ids.iter().map(String::as_str).collect();

      after_turn(&home, &sid, &done_with_id(&input_refs, "turn-1")).await;

      assert!(status_reminders(&home, &sid).is_empty(), "missing reply {missing_reply}");
      assert_eq!(home.nudges(&sid).len(), usize::from(missing_reply), "missing reply {missing_reply}");
    }
  }

  #[tokio::test]
  async fn a_turn_that_recorded_a_status_command_gets_no_status_reminder() {
    for event_type in ["web_status", "web_status_confirmed"] {
      let home = Home::new();
      let sid = home.manager("session", None).await;
      home.append(&sid, json!({"type": event_type, "content": "Status command"})).await;
      let done = home.append(&sid, json!({"type": "master_done", "exit_code": 0, "input_event_ids": []})).await;

      after_turn(&home, &sid, &done).await;

      assert!(status_reminders(&home, &sid).is_empty(), "{event_type}");

      // A status command from the previous turn cannot satisfy the next background turn.
      let next = home.append(&sid, json!({"type": "master_done", "exit_code": 0, "input_event_ids": []})).await;
      after_turn(&home, &sid, &next).await;
      assert_eq!(status_reminders(&home, &sid).len(), 1, "{event_type}");
    }
  }

  #[tokio::test]
  async fn an_errored_turn_gets_no_audit() {
    for (errored_input, expected_nudges) in [("user", 0), ("nudge", 1)] {
      let home = Home::new();
      let sid = home.manager("session", None).await;
      let user_id = id_of(&home.append(&sid, user_event("the question")).await).to_string();
      let input_id = match errored_input {
        "user" => user_id.clone(),
        "nudge" => {
          audit::audit_reply(&home.features, &sid, &master_done(&[user_id.as_str()], 0)).await.unwrap();
          id_of(&home.nudges(&sid)[0]).to_string()
        }
        other => unreachable!("no errored input {other}"),
      };
      let meta = home.meta(&sid).await;

      WebReplyTurnContribution
        .after_turn(&meta, &master_done(&[input_id.as_str()], 1), &config(), &home.features)
        .await
        .unwrap();

      assert_eq!(home.nudges(&sid).len(), expected_nudges, "errored {errored_input} input");
      assert!(home.events(&sid).iter().all(|event| event["type"] != WEB_REPLY_NOTICE), "errored {errored_input} input");
    }
  }
}
