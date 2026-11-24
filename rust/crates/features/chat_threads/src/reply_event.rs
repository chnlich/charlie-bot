//! The chat line a channel package's reply event renders as.
//!
//! Every channel package renders its reply event as one `thread_reply` line that names the platform, so the line's shape
//! has one owner here. The platform's name comes from `thread_entry::ThreadPlatform`.

use charliebot_core::text_repr;
use serde_json::{Value, json};

use crate::thread_entry::ThreadPlatform;

/// The chat line the platform's reply event renders as: its role `thread_reply`, the platform's name, and the reply's
/// content (`reply_line`). An event without content renders empty content.
pub fn reply_line(platform: &ThreadPlatform, event: &Value) -> Value {
  let content = event.get("content").map(text_repr::value_str).unwrap_or_default();
  json!({"role": "thread_reply", "platform": platform.name, "content": content})
}
