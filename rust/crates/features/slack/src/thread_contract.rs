//! The Slack part of the thread reply contract: the thread's address and the read and ack sentences. The shared
//! contract supplies the platform line and joins the part with the common docs.

use charliebot_chat_threads::thread_entry::{IdOrdering, ThreadPlatform};

use crate::metadata::SlackOrigin;

/// The Slack platform as the shared contract reads it.
pub const SLACK: ThreadPlatform = ThreadPlatform {
  name: "slack",
  display_name: "Slack",
  reply_event_type: crate::event_types::SLACK_REPLY,
  reply_command: "charliebot slack reply",
  max_post_chars: 40000,
  scope_doc: "slack_reply_scope.md",
  follow_trigger_prefix: "slack-thread-follow",
  id_key: IdOrdering::Text,
  recheck_after_failed_post: true,
  origin_field: "slack_origin",
  watermark_field: "slack_watermark_ts",
  id_label: "ts",
  mention_key: "mention_ts",
  block_keys: &["channel_id", "thread_ts", "mention_ts"],
  summon_line: "Slack 线程召唤",
};

/// The trigger-label prefix of a session's armed thread-follow record.
const FOLLOW_TRIGGER_PREFIX: &str = "slack-thread-follow";

/// The Slack part of the contract for the thread `origin` names (`thread_contract_part`).
pub fn thread_contract_part(origin: &SlackOrigin) -> String {
  format!(
    "{}\n\
     本线程：channel `{}`、thread_ts `{}`；\
     用 slack 技能读线程（conversations.replies，channel 与 thread_ts 取上面的值）。\
     本次召唤是线程里最新一条 @ 机器人的消息。\n\
     跟帖唤醒（`{FOLLOW_TRIGGER_PREFIX} floor=<ts>`）：用 slack 技能从 floor 标注的消息读起\
     （conversations.replies）；\
     读到的消息用 `charliebot slack ack --message-id <ts> [...]` 确认，本轮沉默也要 ack。",
    SLACK.platform_line(),
    origin.channel_id,
    origin.thread_ts,
  )
}
