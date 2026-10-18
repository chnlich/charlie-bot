//! The web-reply package's turn contribution (`src/features/web_reply/turn_contribution.py`). It carries the reply rules
//! in a web session's manager runs, renders the `web_reply`, `web_reply_notice` and `web_status` events as chat lines,
//! and claims turn-end unread state for web sessions.

use std::fs;
use std::path::Path;

use anyhow::Context;
use charliebot_core::RepoRoot;
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::event_types::{WEB_REPLY, WEB_REPLY_NOTICE, WEB_STATUS};
use charliebot_registry::turn::{EventRenderer, PromptSource, RuleSegment, TurnContribution};
use charliebot_store::metadata::SessionMetadata;
use serde_json::{Value, json};

use crate::reply;

/// The run kind that takes the manager contract (`task_prompts.MANAGER_KINDS`).
const MANAGER_KIND: &str = "manager_turn";
/// The scope of a rule segment's sources (`task_prompts.SCOPE_BASE`).
const SCOPE_BASE: &str = "base";
/// The delivery of a full text segment (`task_prompts.DELIVERY_FULL`).
const DELIVERY_FULL: &str = "full";

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

  fn event_renderers(&self) -> Vec<(&'static str, EventRenderer)> {
    vec![(WEB_REPLY, web_reply_message), (WEB_REPLY_NOTICE, web_reply_notice_message), (WEB_STATUS, web_status_message)]
  }

  fn claims_turn_end_unread(&self, meta: &SessionMetadata) -> bool {
    reply::is_web_session(meta)
  }
}

/// A prompts doc read as `read_text(encoding="utf-8").strip()` reads it: universal newlines, then Python's strip. A
/// missing or unreadable doc fails.
fn rules_text(path: &Path) -> anyhow::Result<String> {
  let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
  Ok(py_strip(&text.replace("\r\n", "\n").replace('\r', "\n")).to_string())
}

/// Python's `str.strip()`: trims what `char::is_whitespace` trims, plus the information separators U+001C to U+001F.
fn py_strip(text: &str) -> &str {
  text.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
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
      paths: PathsConfig { workspace_dirs: Vec::new(), worktree_dir: "/synthetic/worktrees".to_string() },
      backends: BackendsConfig { pool: Vec::new(), options: Vec::new() },
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
}
