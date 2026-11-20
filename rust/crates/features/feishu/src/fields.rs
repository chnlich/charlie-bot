//! The Feishu message, thread and internal-request field names, with the closed value sets a request field takes.
//!
//! One message or thread carries the same field names in the inbound event, the message list, the session origin,
//! the summon block and the read command's output. The CLI builds an internal request and a route reads it. A reader
//! and a writer that disagree on one of these names, or on a value of a closed set, fail silently, so each one is
//! spelled here once.

/// The tenant a message belongs to, from the event header.
pub const TENANT_KEY: &str = "tenant_key";
/// The chat a message was posted in.
pub const CHAT_ID: &str = "chat_id";
/// The root message of the thread a message belongs to.
pub const ROOT_ID: &str = "root_id";
/// The thread (topic) a message belongs to. It is absent until the bot's first reply creates it.
pub const THREAD_ID: &str = "thread_id";
/// A message's own id (`om_…`).
pub const MESSAGE_ID: &str = "message_id";
/// The sender block of a message, in the inbound event and the message list.
pub const SENDER: &str = "sender";
/// The sender's kind inside the sender block: `user` for a person, `app` for the bot.
pub const SENDER_TYPE: &str = "sender_type";
/// A thread message's type, carried in its metadata from the message list's `msg_type` to the readback.
pub const MESSAGE_TYPE: &str = "message_type";
/// A message's creation time, in milliseconds as Feishu writes it.
pub const CREATE_TIME: &str = "create_time";

/// The read request's mode: `chat` reads the group's main chat, `topic` reads one of its topics.
pub const READ_MODE: &str = "mode";
/// The read mode that reads the group's main chat.
pub const READ_MODE_CHAT: &str = "chat";
/// The read mode that reads one topic of the group's main chat.
pub const READ_MODE_TOPIC: &str = "topic";
/// The read modes a read request may name.
pub const READ_MODES: &[&str] = &[READ_MODE_CHAT, READ_MODE_TOPIC];
/// The read request's topic id, required with the `topic` mode.
pub const TOPIC_ID: &str = "topic_id";
/// The read request's page token, which continues a chat or topic listing.
pub const PAGE: &str = "page";
/// The download request's resource key.
pub const FILE_KEY: &str = "file_key";
/// The download request's resource kind.
pub const KIND: &str = "kind";
/// The download kinds the Open API resource endpoint takes.
pub const DOWNLOAD_KINDS: &[&str] = &["image", "file"];
/// The download request's output path.
pub const OUTPUT: &str = "output";
