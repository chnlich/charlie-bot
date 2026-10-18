//! The Discord fields of a session (`src/features/discord/metadata.py`).

use anyhow::Context;
use charliebot_store::metadata::SessionMetadata;
use serde::Deserialize;
use serde_json::Value;

/// The Discord thread a session was summoned from. It is set at creation and never changes.
#[derive(Clone, Debug, Deserialize)]
pub struct DiscordOrigin {
  pub guild_id: String,
  pub parent_channel_id: String,
  pub thread_id: String,
}

/// The session's Discord origin, or `None` when the field is absent or null (`fields_of(meta, "discord").discord_origin`).
/// A stored value that is not a Discord origin fails.
pub fn discord_origin(meta: &SessionMetadata) -> anyhow::Result<Option<DiscordOrigin>> {
  match meta.extra.get("discord_origin") {
    None | Some(Value::Null) => Ok(None),
    Some(value) => DiscordOrigin::deserialize(value).map(Some).context("discord_origin is not a Discord origin"),
  }
}
