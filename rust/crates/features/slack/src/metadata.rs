//! The Slack fields of a session (`src/features/slack/metadata.py`).

use anyhow::Context;
use charliebot_store::metadata::SessionMetadata;
use serde::Deserialize;
use serde_json::Value;

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
  match meta.extra.get("slack_origin") {
    None | Some(Value::Null) => Ok(None),
    Some(value) => SlackOrigin::deserialize(value).map(Some).context("slack_origin is not a Slack origin"),
  }
}
