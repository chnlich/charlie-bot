//! The Feishu part of the thread reply contract: the platform line and the read sentences. The shared contract joins it
//! with the common docs.

use charliebot_chat_threads::thread_entry::{IdOrdering, LINKED_PAGES_LINE, ThreadPlatform};

/// The command the reply-format contract names for posting a reply.
pub const REPLY_COMMAND: &str = "charliebot feishu reply";
/// The per-message character limit, which is also the chunking target. Feishu caps a text body at 150 KB.
pub const MAX_POST_CHARS: usize = 30000;
/// The command the contract names for reading the thread server-side.
pub const READ_COMMAND: &str = "charliebot feishu read";
/// The trigger-label prefix of a session's armed thread-follow record.
pub const FOLLOW_TRIGGER_PREFIX: &str = "feishu-thread-follow";

/// The Feishu platform as the shared contract reads it.
pub const FEISHU: ThreadPlatform = ThreadPlatform {
  name: "feishu",
  display_name: "Feishu",
  reply_event_type: crate::event_types::FEISHU_REPLY,
  reply_command: REPLY_COMMAND,
  max_post_chars: MAX_POST_CHARS,
  scope_doc: "feishu_reply_scope.md",
  follow_trigger_prefix: FOLLOW_TRIGGER_PREFIX,
  id_key: IdOrdering::TimeThenId,
  recheck_after_failed_post: false,
  origin_field: "feishu_origin",
  watermark_field: "feishu_watermark_id",
  id_label: "id",
  mention_key: "mention_id",
  block_keys: &["chat_id", "root_id", "mention_id"],
  summon_line: "飞书 线程召唤",
};

/// The Feishu part of the contract (`thread_contract_part`). It names no thread: the read command reaches it.
pub fn thread_contract_part() -> String {
  format!(
    "{}\n\
     用 `{READ_COMMAND}` 读这条话题（服务端代读，返回 bot 在这条话题里能看到的消息，未读的随之记为已读）。\
     读回里 `summon: true` 的消息就是本轮要回答的召唤。\n\
     跟帖唤醒（`{FOLLOW_TRIGGER_PREFIX} floor=<id>`）：用 `{READ_COMMAND}` 读话题里的新消息\
     （本次返回的未读消息随之记为已读，本轮沉默也要先读）。",
    platform_line()
  )
}

/// The platform line every Feishu contract part opens with.
fn platform_line() -> String {
  format!(
    "Platform: Feishu. Reply command: `{REPLY_COMMAND} --file <path>`. \
     Per-message limit: {MAX_POST_CHARS} characters. {LINKED_PAGES_LINE}"
  )
}
