//! The For-you rules: the roles the For-you layer shows, the error-row test, the page's `FOR_YOU_RULE`, and the older
//! rows a session bootstrap carries before its first screen. The web reply dialog, the page and the bootstrap read this
//! one module.

use charliebot_core::event_types::{USER, WEB_REPLY, WEB_REPLY_NOTICE};
use serde_json::{Value, json};

use crate::{Store, StoreResult};

/// The reply layer's roles: the user's messages, the agent's web replies and the notices that answer them.
pub const REPLY_ROLES: [&str; 3] = [USER, WEB_REPLY, WEB_REPLY_NOTICE];
/// The role of the draft card, which the page shows from the pending draft and no event carries.
pub const DRAFT_ROLE: &str = "draft";
/// The role an error row carries, and the text prefix that marks it. The aggregator's ASSISTANT_ERROR, ERROR and
/// RUN_ERROR arms write the prefix.
pub const ERROR_ROLE: &str = "system";
pub const ERROR_PREFIX: &str = "Error: ";
/// The most For-you rows one bootstrap carries before its first screen.
pub const FOR_YOU_MAX_ROWS: usize = 10;
/// The most event lines one bootstrap's scan reads back from its first screen.
pub const FOR_YOU_SCAN_MAX_LINES: usize = 2_000;
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
  message["role"].as_str() == Some(ERROR_ROLE)
    && message["content"].as_str().is_some_and(|text| text.starts_with(ERROR_PREFIX))
}

/// The page's `FOR_YOU_RULE`: the roles the page shows, the draft included, and the error-row test.
pub fn page_rule() -> Value {
  json!({
    "roles": REPLY_ROLES.iter().copied().chain([DRAFT_ROLE]).collect::<Vec<_>>(),
    "error": {"role": ERROR_ROLE, "prefix": ERROR_PREFIX},
  })
}

impl Store {
  /// The For-you rows before the event index `before`, oldest first.
  ///
  /// The scan walks back from `before` in windows of event lines, archived lines included, and keeps the newest
  /// reply-layer and error rows. It stops at [`FOR_YOU_MAX_ROWS`] rows, at [`FOR_YOU_SCAN_MAX_LINES`] lines, or when
  /// the next row would take the JSON past [`FOR_YOU_MAX_JSON_BYTES`]. The newest row is always kept.
  pub fn for_you_rows_before(&self, session_id: &str, before: usize) -> StoreResult<Vec<Value>> {
    let mut newest_first: Vec<Value> = Vec::new();
    let mut json_bytes = 0;
    let mut end = before;
    let mut scanned = 0;
    'scan: while end > 0 && scanned < FOR_YOU_SCAN_MAX_LINES && newest_first.len() < FOR_YOU_MAX_ROWS {
      let lines = FOR_YOU_SCAN_WINDOW_LINES.min(FOR_YOU_SCAN_MAX_LINES - scanned).min(end);
      let start = end - lines;
      let (events, _) = self.load_chat_events_range(session_id, start, end)?;
      for message in self.events_to_messages_at(&events, start)?.into_iter().rev() {
        if !is_reply_row(&message) && !is_error_row(&message) {
          continue;
        }
        let row_bytes = message.to_string().len() + 1;
        if !newest_first.is_empty() && json_bytes + row_bytes > FOR_YOU_MAX_JSON_BYTES {
          break 'scan;
        }
        json_bytes += row_bytes;
        newest_first.push(message);
        if newest_first.len() == FOR_YOU_MAX_ROWS {
          break 'scan;
        }
      }
      scanned += lines;
      end = start;
    }
    newest_first.reverse();
    Ok(newest_first)
  }
}
