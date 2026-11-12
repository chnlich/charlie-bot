//! The Feishu message and thread field names.
//!
//! One message or thread carries the same field names in the inbound event, the message list, the session origin,
//! the summon block and the read command's output. A reader and a writer that disagree on one of them fail silently,
//! so each name is spelled here once.

/// The tenant a message belongs to, from the event header.
pub const TENANT_KEY: &str = "tenant_key";
/// The chat a message was posted in.
pub const CHAT_ID: &str = "chat_id";
/// The root message of the thread a message belongs to.
pub const ROOT_ID: &str = "root_id";
/// The thread (topic) a message belongs to; absent until the bot's first reply creates it.
pub const THREAD_ID: &str = "thread_id";
/// A message's own id (`om_…`).
pub const MESSAGE_ID: &str = "message_id";
/// The sender block of a message, in the inbound event and the message list.
pub const SENDER: &str = "sender";
/// The sender's kind inside the sender block: `user` for a person, `app` for the bot.
pub const SENDER_TYPE: &str = "sender_type";
/// A thread message's type, carried in its metadata from the message list's `msg_type` to the readback.
pub const MESSAGE_TYPE: &str = "message_type";
/// The summon block's id of the message that mentions the bot.
pub const MENTION_ID: &str = "mention_id";
/// A message's creation time, in milliseconds as Feishu writes it.
pub const CREATE_TIME: &str = "create_time";
