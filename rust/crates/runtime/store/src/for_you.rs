//! The For-you rules: the roles the For-you layer shows, the thread rows a chat thread adds, the error-row test, the
//! page's `FOR_YOU_RULE`, and the older rows a session bootstrap carries before its first screen. The web reply dialog,
//! the page and the bootstrap read this one module. The page reads the rule's `thread` entry for the same thread rows
//! the bootstrap carries, so a row shows on first paint and on a live update by one definition.

use charliebot_core::event_types::{AGENT_MESSAGE, NUDGE_OF, THREAD_PLATFORM_KEYS, USER, WEB_REPLY, WEB_REPLY_NOTICE};
use serde_json::{Value, json};

use crate::{Store, StoreResult, run_logs};

/// The reply layer's roles: the user's messages, the agent's web replies and the notices that answer them. The web
/// reply dialog reads these alone.
pub const REPLY_ROLES: [&str; 3] = [USER, WEB_REPLY, WEB_REPLY_NOTICE];
/// The role of a reply a chat thread received. Its `platform` names the platform and its `content` is the reply.
pub const THREAD_REPLY_ROLE: &str = "thread_reply";
/// The role of the draft card, which the page shows from the pending draft and no event carries.
pub const DRAFT_ROLE: &str = "draft";
/// The roles the page's For-you layer shows: the reply layer, the thread replies and the draft card.
pub const FOR_YOU_ROLES: [&str; 5] = [USER, WEB_REPLY, WEB_REPLY_NOTICE, THREAD_REPLY_ROLE, DRAFT_ROLE];
/// The role of a system line: an error row, or a legacy thread reply line.
pub const SYSTEM_ROLE: &str = "system";
/// The text an error row starts with. The aggregator's ASSISTANT_ERROR, ERROR and RUN_ERROR arms write it.
pub const ERROR_PREFIX: &str = "Error: ";
/// The text a legacy thread reply line starts with, before its platform label and `: `. Older history holds these as
/// system lines; they read as thread replies, so no history is rewritten.
pub const LEGACY_THREAD_PREFIX: &str = "Posted to ";
/// The most For-you rows one bootstrap carries before its first screen.
pub const FOR_YOU_MAX_ROWS: usize = 10;
/// The most rendered event lines one bootstrap's scan reads back from its first screen. A progress line renders no row,
/// so it does not count here.
pub const FOR_YOU_SCAN_MAX_LINES: usize = 2_000;
/// The most event lines one bootstrap's scan reads back from its first screen, progress lines included. It bounds the
/// read cost of a progress flood between the replies.
pub const FOR_YOU_SCAN_MAX_READ_LINES: usize = 16_384;
/// The event lines one scan read takes at a time.
const FOR_YOU_SCAN_WINDOW_LINES: usize = 200;
/// The JSON bytes the older rows may take. The newest row is kept whatever its size.
pub const FOR_YOU_MAX_JSON_BYTES: usize = 16 * 1024;

/// Whether a message is a reply-layer row: its role is one of [`REPLY_ROLES`].
pub fn is_reply_row(message: &Value) -> bool {
  message["role"].as_str().is_some_and(|role| REPLY_ROLES.contains(&role))
}

/// Whether a message is an error row: a system message whose text starts with [`ERROR_PREFIX`].
pub fn is_error_row(message: &Value) -> bool {
  message["role"].as_str() == Some(SYSTEM_ROLE)
    && message["content"].as_str().is_some_and(|text| text.starts_with(ERROR_PREFIX))
}

/// Whether a message is a platform summon: an `agent_message` whose `slack`, `discord` or `feishu` block has no
/// `nudge_of`. A nudge wakes a session on a thread it already joined, so it is not a summon. The aggregator copies each
/// platform block into the message, so this reads the rendered row the way the event form reads the event.
pub fn is_summon_row(message: &Value) -> bool {
  message["role"].as_str() == Some(AGENT_MESSAGE)
    && THREAD_PLATFORM_KEYS
      .iter()
      .any(|key| message[*key].as_object().is_some_and(|block| !block.contains_key(NUDGE_OF)))
}

/// Whether a message is a legacy thread reply: a system line that starts with [`LEGACY_THREAD_PREFIX`] and a platform
/// label, as `Posted to Slack: ...` does.
pub fn is_legacy_thread_reply(message: &Value) -> bool {
  message["role"].as_str() == Some(SYSTEM_ROLE)
    && message["content"].as_str().is_some_and(|text| legacy_thread_platform(text).is_some())
}

/// Whether a message is a thread row: a thread reply, a legacy thread reply, or a platform summon.
pub fn is_thread_row(message: &Value) -> bool {
  message["role"].as_str() == Some(THREAD_REPLY_ROLE) || is_legacy_thread_reply(message) || is_summon_row(message)
}

/// Whether a message is a For-you row: a reply-layer row, a thread row or an error row.
pub fn is_for_you_row(message: &Value) -> bool {
  is_reply_row(message) || is_thread_row(message) || is_error_row(message)
}

/// The label a platform key takes in a legacy line: its key with a capital first letter (`slack` reads `Slack`).
fn platform_label(key: &str) -> String {
  let mut chars = key.chars();
  chars.next().map(|first| first.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

/// The platform a legacy thread reply line names, or `None` for any other text.
fn legacy_thread_platform(text: &str) -> Option<&'static str> {
  let rest = text.strip_prefix(LEGACY_THREAD_PREFIX)?;
  let (label, _) = rest.split_once(": ")?;
  THREAD_PLATFORM_KEYS.iter().copied().find(|key| platform_label(key) == label)
}

/// The page's `FOR_YOU_RULE`: the roles the page shows, the error-row test, and the data of the thread rows. The page
/// reads the thread entry for its platform list, its legacy prefix, its summon role and its nudge key, so the page's
/// filter and [`is_thread_row`] read the same values. `web/tests/for_you_rule.json` holds this object for the JS tests.
pub fn page_rule() -> Value {
  json!({
    "roles": FOR_YOU_ROLES,
    "error": {"role": SYSTEM_ROLE, "prefix": ERROR_PREFIX},
    "thread": {
      "reply_role": THREAD_REPLY_ROLE,
      "platforms": THREAD_PLATFORM_KEYS,
      "legacy_prefix": LEGACY_THREAD_PREFIX,
      "summon_role": AGENT_MESSAGE,
      "nudge_key": NUDGE_OF,
    },
  })
}

impl Store {
  /// The For-you rows before the event index `before`, oldest first.
  ///
  /// The scan walks back from `before` in windows of event lines, archived lines included, and keeps the newest
  /// For-you rows (see [`is_for_you_row`]). A progress line (a thinking-token event) renders no row, so it counts only
  /// against [`FOR_YOU_SCAN_MAX_READ_LINES`]; every other line counts against both bounds. The scan stops at
  /// [`FOR_YOU_MAX_ROWS`] rows, at either bound, or when the next row would take the JSON past
  /// [`FOR_YOU_MAX_JSON_BYTES`]. The newest row is always kept.
  pub fn for_you_rows_before(&self, session_id: &str, before: usize) -> StoreResult<Vec<Value>> {
    let mut newest_first: Vec<Value> = Vec::new();
    let mut json_bytes = 2; // The array's opening and closing brackets.
    let mut end = before;
    let mut rendered = 0;
    let mut read = 0;
    'scan: while end > 0
      && rendered < FOR_YOU_SCAN_MAX_LINES
      && read < FOR_YOU_SCAN_MAX_READ_LINES
      && newest_first.len() < FOR_YOU_MAX_ROWS
    {
      // The window takes no more lines than either bound still allows, so its rendered lines stay within the first.
      let lines = FOR_YOU_SCAN_WINDOW_LINES
        .min(FOR_YOU_SCAN_MAX_LINES - rendered)
        .min(FOR_YOU_SCAN_MAX_READ_LINES - read)
        .min(end);
      let start = end - lines;
      let (events, _) = self.load_chat_events_range(session_id, start, end)?;
      let progress = events.iter().filter(|event| run_logs::is_thinking_tokens_event(*event)).count();
      rendered += lines - progress;
      read += lines;
      for message in self.events_to_messages_at(&events, start)?.into_iter().rev() {
        if !is_for_you_row(&message) {
          continue;
        }
        let row_bytes = message.to_string().len();
        let separator_bytes = if newest_first.is_empty() { 0 } else { 1 };
        if !newest_first.is_empty() && json_bytes + separator_bytes + row_bytes > FOR_YOU_MAX_JSON_BYTES {
          break 'scan;
        }
        json_bytes += separator_bytes + row_bytes;
        newest_first.push(message);
        if newest_first.len() == FOR_YOU_MAX_ROWS {
          break 'scan;
        }
      }
      end = start;
    }
    newest_first.reverse();
    Ok(newest_first)
  }
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use charliebot_core::event_types::{AGENT_MESSAGE, BACKEND_SWITCHED, USER};
  use serde_json::json;

  use super::*;
  use crate::hooks::{EmptyRuntimeEventHooks, EventRenderer, RuntimeEventHooks};
  use crate::metadata::{SessionMetadata, TaskProfile};
  use crate::store::test_support::make_test_store_with_runtime;

  /// The rule the JS harness loads: the page's `FOR_YOU_RULE`, as the server renders it.
  const FOR_YOU_RULE_FIXTURE: &str = include_str!("../../../../../web/tests/for_you_rule.json");

  #[test]
  fn the_page_rule_is_the_fixture_the_js_harness_loads() {
    let fixture: Value = serde_json::from_str(FOR_YOU_RULE_FIXTURE).unwrap();
    assert_eq!(page_rule(), fixture);
  }

  #[test]
  fn the_page_rule_shows_thread_replies_and_summons_in_the_for_you_roles() {
    let rule = page_rule();
    assert!(rule["roles"].as_array().unwrap().iter().any(|role| role == THREAD_REPLY_ROLE));
    assert_eq!(rule["thread"]["platforms"], json!(["slack", "discord", "feishu"]));
    assert_eq!(rule["thread"]["legacy_prefix"], LEGACY_THREAD_PREFIX);
  }

  #[test]
  fn a_thread_reply_row_is_a_for_you_row() {
    let reply = json!({"role": THREAD_REPLY_ROLE, "platform": "slack", "content": "answer"});
    assert!(is_thread_row(&reply));
    assert!(is_for_you_row(&reply));
    assert!(!is_reply_row(&reply), "the web reply dialog reads the reply layer alone");
  }

  #[test]
  fn a_platform_summon_is_a_for_you_row_and_a_nudge_is_not() {
    let summon = json!({"role": AGENT_MESSAGE, "content": "hi", "feishu": {"chat_id": "oc_1"}});
    assert!(is_summon_row(&summon));
    assert!(is_for_you_row(&summon));

    let nudge = json!({"role": AGENT_MESSAGE, "content": "hi", "slack": {"thread_ts": "1", "nudge_of": "2"}});
    assert!(!is_summon_row(&nudge));
    assert!(!is_for_you_row(&nudge));

    let plain_agent_message = json!({"role": AGENT_MESSAGE, "content": "hi"});
    assert!(!is_summon_row(&plain_agent_message));
  }

  #[test]
  fn a_legacy_posted_to_line_is_a_for_you_row_only_for_a_platform_label() {
    for label in ["Slack", "Discord", "Feishu"] {
      let line = json!({"role": SYSTEM_ROLE, "content": format!("Posted to {label}: reply text")});
      assert!(is_legacy_thread_reply(&line), "{label} names a platform");
      assert!(is_for_you_row(&line));
    }
    let empty_reply = json!({"role": SYSTEM_ROLE, "content": "Posted to Slack: "});
    assert!(is_legacy_thread_reply(&empty_reply), "an empty reply keeps its label");
    for text in ["Posted to Teams: reply", "Posted to Slack reply", "Backend switched: a to b"] {
      assert!(!is_legacy_thread_reply(&json!({"role": SYSTEM_ROLE, "content": text})), "{text}");
    }
  }

  #[test]
  fn an_error_row_and_the_reply_layer_stay_for_you_rows() {
    assert!(is_for_you_row(&json!({"role": SYSTEM_ROLE, "content": "Error: failed"})));
    assert!(is_for_you_row(&json!({"role": WEB_REPLY, "content": "answer"})));
    assert!(is_for_you_row(&json!({"role": USER, "content": "question"})));
    assert!(!is_for_you_row(&json!({"role": SYSTEM_ROLE, "content": "Backend switched"})));
  }

  /// Renders a test thread reply as a `thread_reply` row, an old `Posted to Discord:` event as its legacy system line,
  /// and a web reply as its `web_reply` row. The other hooks stay empty.
  struct ThreadRenderHook;

  impl RuntimeEventHooks for ThreadRenderHook {
    fn copy_on_fork(&self, _parent_dir: &std::path::Path, _child_dir: &std::path::Path) -> StoreResult<()> {
      Ok(())
    }

    fn event_renderers(&self) -> StoreResult<Vec<(String, Arc<dyn EventRenderer>)>> {
      let thread_reply: Arc<dyn EventRenderer> = Arc::new(|event: &Value| {
        Ok(Some(json!({"role": THREAD_REPLY_ROLE, "platform": "slack", "content": event["content"].clone()})))
      });
      let legacy_reply: Arc<dyn EventRenderer> = Arc::new(|event: &Value| {
        let text = event["content"].as_str().unwrap_or("");
        Ok(Some(json!({"role": SYSTEM_ROLE, "content": format!("Posted to Discord: {text}")})))
      });
      let web_reply: Arc<dyn EventRenderer> =
        Arc::new(|event: &Value| Ok(Some(json!({"role": WEB_REPLY, "content": event["content"].clone()}))));
      Ok(vec![
        ("test_thread_reply".to_string(), thread_reply),
        ("test_legacy_discord_reply".to_string(), legacy_reply),
        (WEB_REPLY.to_string(), web_reply),
      ])
    }

    fn mark_sidebar_dirty(&self, session_id: &str) {
      EmptyRuntimeEventHooks.mark_sidebar_dirty(session_id)
    }

    fn drop_session_runtime_state(&self, session_id: &str) {
      EmptyRuntimeEventHooks.drop_session_runtime_state(session_id)
    }
  }

  /// A manager session's metadata file, written as the session store reads it, so its events append.
  fn write_manager_session(store: &Store, session_id: &str) {
    let metadata = SessionMetadata::new(session_id, "synthetic", TaskProfile::Manager);
    let path = store.metadata_path(session_id);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, metadata.to_json_string(&[], true)).unwrap();
  }

  #[tokio::test]
  async fn the_bootstrap_carries_a_thread_sessions_replies_and_summons_in_log_order() {
    let (_home, store) = make_test_store_with_runtime(Arc::new(ThreadRenderHook));
    write_manager_session(&store, "thread-manager");
    let base = store.get_chat_event_count_sync("thread-manager").unwrap();
    let events = [
      json!({"type": USER, "content": "question"}),
      json!({"type": AGENT_MESSAGE, "content": "summon text", "slack": {"thread_ts": "1.0", "channel_id": "C1"}}),
      json!({"type": "test_thread_reply", "content": "thread answer"}),
      json!({"type": AGENT_MESSAGE, "content": "nudge", "slack": {"thread_ts": "1.0", "nudge_of": "9"}}),
      json!({"type": "test_legacy_discord_reply", "content": "old answer"}),
      json!({"type": BACKEND_SWITCHED, "from": "a", "to": "b"}),
      json!({"type": WEB_REPLY, "content": "web answer"}),
    ];
    for event in events {
      store.append_chat_event("thread-manager", event).await.unwrap();
    }

    let rows = store.for_you_rows_before("thread-manager", base + 7).unwrap();
    let roles: Vec<&str> = rows.iter().map(|row| row["role"].as_str().unwrap()).collect();
    assert_eq!(roles, [USER, AGENT_MESSAGE, THREAD_REPLY_ROLE, SYSTEM_ROLE, WEB_REPLY]);
    assert!(rows.iter().all(is_for_you_row));
    assert_eq!(rows[1]["slack"]["thread_ts"], "1.0", "the summon keeps its platform block");
    assert_eq!(rows[3]["content"], "Posted to Discord: old answer");
  }

  /// Writes `lines` into the session's live log in one write. A fixture of thousands of lines is too slow to append one
  /// event at a time in a unit test.
  fn write_live_lines(store: &Store, session_id: &str, lines: &[Value]) {
    let path = store.chat_events_path(session_id);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let body: String = lines.iter().map(|line| format!("{line}\n")).collect();
    std::fs::write(path, body).unwrap();
  }

  /// A thinking-token progress line, as Claude Code's stream writes it.
  fn progress_line(index: usize) -> Value {
    json!({"type": "system", "subtype": "thinking_tokens", "estimated_tokens": index})
  }

  /// The newest replies reach the older rows behind a flood of 5,000 progress lines. A progress line renders no row, so
  /// it does not spend the rendered-line bound.
  #[test]
  fn the_newest_replies_reach_the_older_rows_behind_a_progress_flood() {
    let (_home, store) = make_test_store_with_runtime(Arc::new(ThreadRenderHook));
    write_manager_session(&store, "progress-flood");
    let mut lines: Vec<Value> = (0..12).map(|turn| json!({"type": USER, "content": format!("reply {turn}")})).collect();
    lines.extend((0..5_000).map(progress_line));
    write_live_lines(&store, "progress-flood", &lines);

    let rows = store.for_you_rows_before("progress-flood", lines.len()).unwrap();
    let contents: Vec<String> = rows.iter().map(|row| row["content"].as_str().unwrap().to_string()).collect();
    assert_eq!(contents, (2..12).map(|turn| format!("reply {turn}")).collect::<Vec<_>>());
  }

  /// The scan reads at most [`FOR_YOU_SCAN_MAX_READ_LINES`] lines, progress lines included, so a flood longer than that
  /// bound hides the reply behind it.
  #[test]
  #[ignore = "integration"]
  fn a_progress_flood_past_the_read_bound_hides_the_reply_behind_it() {
    let (_home, store) = make_test_store_with_runtime(Arc::new(ThreadRenderHook));
    write_manager_session(&store, "progress-bound");
    let mut lines = vec![json!({"type": USER, "content": "far"})];
    lines.extend((0..FOR_YOU_SCAN_MAX_READ_LINES).map(progress_line));
    write_live_lines(&store, "progress-bound", &lines);

    let rows = store.for_you_rows_before("progress-bound", lines.len()).unwrap();
    assert!(rows.is_empty(), "the reply sits one line past the read bound");
  }
}
