//! The Discord fields of a session.

use charliebot_registry::thread_origin;
use charliebot_store::metadata::SessionMetadata;
use serde::Deserialize;

/// The session-metadata field holding the Discord thread a session was summoned from.
pub const DISCORD_ORIGIN: &str = "discord_origin";
/// The session-metadata field holding the Discord id the session's turn has read up to.
pub const DISCORD_WATERMARK_ID: &str = "discord_watermark_id";

/// The Discord thread a session was summoned from. It is set at creation and never changes.
#[derive(Clone, Debug, Deserialize)]
pub struct DiscordOrigin {
  pub guild_id: String,
  pub parent_channel_id: String,
  pub thread_id: String,
}

/// The session's Discord origin, or `None` when the field is absent or null
/// (`fields_of(meta, "discord").discord_origin`). A stored value that is not a Discord origin fails.
pub fn discord_origin(meta: &SessionMetadata) -> anyhow::Result<Option<DiscordOrigin>> {
  thread_origin::read_origin(meta, DISCORD_ORIGIN, "a Discord origin")
}
