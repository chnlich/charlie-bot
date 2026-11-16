//! The chat line a channel package's reply event renders as.
//!
//! Every channel package renders its reply event with one system line naming the platform, so the sentence has one
//! owner here. The platform's facts, its display name included, come from `thread_entry::ThreadPlatform`.

use charliebot_core::text_repr;
use serde_json::{Value, json};

use crate::thread_entry::ThreadPlatform;

/// The chat line the platform's reply event renders as: `Posted to <display name>: <content>` (`reply_line`).
///
/// The event's `content` renders as text, and an event without one renders empty.
pub fn reply_line(platform: &ThreadPlatform, event: &Value) -> Value {
  let content = event.get("content").map(text_repr::value_str).unwrap_or_default();
  json!({"role": "system", "content": format!("Posted to {}: {content}", platform.display_name)})
}
