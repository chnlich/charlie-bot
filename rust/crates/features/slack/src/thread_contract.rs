//! The Slack part of the thread reply contract (`src/features/slack/slack_listener.py`): the platform line, the
//! thread's address, and the read and ack sentences. The shared contract joins it with the common docs.

use charliebot_chat_threads::thread_entry::{IdOrdering, LINKED_PAGES_LINE, ThreadPlatform};

use crate::metadata::SlackOrigin;

/// The Slack platform as the shared contract reads it (`slack_listener.SLACK`).
pub const SLACK: ThreadPlatform = ThreadPlatform {
  name: "slack",
  display_name: "Slack",
  reply_event_type: crate::event_types::SLACK_REPLY,
  reply_command: "charliebot slack reply",
  max_post_chars: 40000,
  scope_doc: "slack_reply_scope.md",
  follow_trigger_prefix: "slack-thread-follow",
  id_key: IdOrdering::Text,
  origin_field: "slack_origin",
  watermark_field: "slack_watermark_ts",
  id_label: "ts",
  mention_key: "mention_ts",
  block_keys: &["channel_id", "thread_ts", "mention_ts"],
  summon_line: "Slack 线程召唤",
};

/// The command the reply-format contract names for posting a reply (`_REPLY_COMMAND`).
const REPLY_COMMAND: &str = "charliebot slack reply";
/// Slack's per-message text limit, as the contract states it (`_MAX_POST_CHARS`).
const MAX_POST_CHARS: usize = 40000;
/// The trigger-label prefix of a session's armed thread-follow record (`_FOLLOW_TRIGGER_PREFIX`).
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
    platform_line(),
    origin.channel_id,
    origin.thread_ts,
  )
}

/// The platform line every Slack contract part opens with (`_PLATFORM_LINE`).
fn platform_line() -> String {
  format!(
    "Platform: Slack. Reply command: `{REPLY_COMMAND} --file <path>`. \
     Per-message limit: {MAX_POST_CHARS} characters. {LINKED_PAGES_LINE}"
  )
}
