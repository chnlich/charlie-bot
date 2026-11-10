//! The Slack fields of a session.

use charliebot_registry::thread_origin;
use charliebot_store::metadata::SessionMetadata;
use serde::Deserialize;

/// The Slack thread a session was summoned from. It is set at creation and never changes.
#[derive(Clone, Debug, Deserialize)]
pub struct SlackOrigin {
  pub team_id: String,
  pub channel_id: String,
  pub thread_ts: String,
}

/// The session's Slack origin, or `None` when the field is absent or null (`fields_of(meta, "slack").slack_origin`).
/// A stored value that is not a Slack origin fails.
pub fn slack_origin(meta: &SessionMetadata) -> anyhow::Result<Option<SlackOrigin>> {
  thread_origin::read_origin(meta, "slack_origin", "a Slack origin")
}
