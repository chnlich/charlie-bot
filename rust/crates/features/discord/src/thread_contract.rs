//! The Discord part of the thread reply contract (`discord_listener.thread_contract_part`): the platform line and the read
//! sentences. The shared contract joins it with the common docs.

use charliebot_chat_threads::thread_entry::LINKED_PAGES_LINE;

use crate::discord_listener::{FOLLOW_TRIGGER_PREFIX, MAX_POST_CHARS, READ_COMMAND, REPLY_COMMAND};

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
