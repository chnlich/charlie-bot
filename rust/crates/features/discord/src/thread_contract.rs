//! The Discord part of the thread reply contract (`src/features/discord/discord_listener.py`): the platform line and
//! the read sentences. The shared contract joins it with the common docs.

use charliebot_chat_threads::thread_entry::{IdOrdering, LINKED_PAGES_LINE, ThreadPlatform};

/// The Discord platform as the shared contract reads it (`discord_listener.DISCORD`).
pub const DISCORD: ThreadPlatform = ThreadPlatform {
  name: "discord",
  display_name: "Discord",
  reply_event_type: crate::event_types::DISCORD_REPLY,
  reply_command: "charliebot discord reply",
  max_post_chars: 2000,
  scope_doc: "discord_reply_scope.md",
  follow_trigger_prefix: "discord-thread-follow",
  id_key: IdOrdering::Snowflake,
  origin_field: "discord_origin",
  watermark_field: "discord_watermark_id",
  id_label: "id",
  mention_key: "mention_id",
  block_keys: &["guild_id", "channel_id", "thread_id", "mention_id"],
  summon_line: "Discord 线程召唤",
};

/// The command the reply-format contract names for posting a reply (`_REPLY_COMMAND`).
const REPLY_COMMAND: &str = "charliebot discord reply";
/// Discord's per-message text limit, which is also the chunking target (`_MAX_POST_CHARS`).
const MAX_POST_CHARS: usize = 2000;
/// The command the contract names for reading the thread server-side (`_READ_COMMAND`).
const READ_COMMAND: &str = "charliebot discord read";
/// The trigger-label prefix of a session's armed thread-follow record (`_FOLLOW_TRIGGER_PREFIX`).
const FOLLOW_TRIGGER_PREFIX: &str = "discord-thread-follow";

/// The Discord part of the contract (`thread_contract_part`). It names no thread: the read command reaches it.
pub fn thread_contract_part() -> String {
  format!(
    "{}\n\
     用 `{READ_COMMAND}` 读这条线程（服务端代读，返回 bot 在这条线程里能看到的消息，未读的随之记为已读）。\
     读回里 `summon: true` 的消息就是本轮要回答的召唤。\n\
     跟帖唤醒（`{FOLLOW_TRIGGER_PREFIX} floor=<id>`）：用 `{READ_COMMAND}` 读线程里的新消息\
     （本次返回的未读消息随之记为已读，本轮沉默也要先读）。",
    platform_line()
  )
}

/// The platform line every Discord contract part opens with (`_PLATFORM_LINE`).
fn platform_line() -> String {
  format!(
    "Platform: Discord. Reply command: `{REPLY_COMMAND} --file <path>`. \
     Per-message limit: {MAX_POST_CHARS} characters. {LINKED_PAGES_LINE}"
  )
}
