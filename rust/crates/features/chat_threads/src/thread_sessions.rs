//! The thread-session predicate (`src/features/chat_threads/thread_sessions.py`).
//!
//! A thread session answers one Slack or Discord thread: its metadata carries that platform's origin field, set at
//! summon creation and never mutated. The turn contribution and the sidebar classify through [`is_thread_session`],
//! so the test has one definition.

use charliebot_store::metadata::SessionMetadata;

/// True when `meta` carries a Slack or Discord thread origin: `slack_origin` or `discord_origin` is present and not
/// null.
pub fn is_thread_session(meta: &SessionMetadata) -> bool {
  let has_origin = |field: &str| meta.extra(field).is_some_and(|origin| !origin.is_null());
  has_origin("slack_origin") || has_origin("discord_origin")
}
