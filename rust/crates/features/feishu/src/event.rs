//! The `im.message.receive_v1` event: its parsing, and the drop rules that decide whether it is a summon, a thread
//! follow or traffic to drop. A summon is a group message that mentions the bot from a human sender. A follow is a
//! message from a human sender inside a thread of a group. Whether the group is admitted at all is decided after this
//! module, from the group's members (`owners`). Every other event is dropped before the shared core runs. Those drops
//! are bot and app messages, one-to-one chats, and group messages that neither mention the bot nor sit in a thread.

use anyhow::{Context, bail};
use serde_json::Value;

use crate::fields::{CHAT_ID, CREATE_TIME, MESSAGE_ID, ROOT_ID, SENDER, SENDER_TYPE, TENANT_KEY, THREAD_ID};

/// The event type this module parses.
pub const RECEIVE_EVENT: &str = "im.message.receive_v1";

/// The message fields the rules and the adapter read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
  pub tenant_key: String,
  pub chat_id: String,
  /// The message's own id (`om_…`).
  pub message_id: String,
  /// The root message of the thread the message belongs to; None when the event names none.
  pub root_id: Option<String>,
  /// The thread's id (`omt_…`); None while the message is not in a thread.
  pub thread_id: Option<String>,
  /// The creation time in milliseconds, as Feishu writes it.
  pub create_time: String,
  /// The sender's open_id.
  pub sender_open_id: String,
  pub mentions: Vec<Mention>,
}

/// One `@` mention in a message: the placeholder key the content uses, the mentioned open_id and its name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mention {
  pub key: String,
  pub open_id: String,
  pub name: String,
}

/// The verdict on one event.
#[derive(Debug, PartialEq, Eq)]
pub enum Inbound {
  /// A human mentioned the bot in a group.
  Summon(Message),
  /// A human posted in a thread of a group, without a mention of the bot.
  Follow(Message),
  /// The event is not for the thread entrypoint.
  Drop(Drop),
}

/// Why an event is dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Drop {
  /// The event is another type.
  OtherEvent,
  /// The sender is a bot or the app itself, not a user.
  NotUser,
  /// The message comes from a one-to-one chat.
  NotGroup,
  /// The group message neither mentions the bot nor sits in a thread.
  NoMentionOutsideThread,
  /// The group has no person listed in `feishu.allowed_users` among its members. Set by the listener, after
  /// classification.
  NoOwnerInChat,
}

/// The classification of one event against the bot's open_id (`classify`). A malformed event fails.
pub fn classify(event: &Value, bot_open_id: &str) -> anyhow::Result<Inbound> {
  if text_at(event, &["header", "event_type"])? != RECEIVE_EVENT {
    return Ok(Inbound::Drop(Drop::OtherEvent));
  }
  let body = event.get("event").context("the event has no event object")?;
  let sender = body.get(SENDER).context("the event has no sender")?;
  if text_at(sender, &[SENDER_TYPE])? != "user" {
    return Ok(Inbound::Drop(Drop::NotUser));
  }
  let sender_open_id = text_at(sender, &["sender_id", "open_id"])?;
  let raw = body.get("message").context("the event has no message")?;
  if text_at(raw, &["chat_type"])? != "group" {
    return Ok(Inbound::Drop(Drop::NotGroup));
  }
  let mentions = mentions_of(raw)?;
  let mentioned = mentions.iter().any(|mention| mention.open_id == bot_open_id);
  let message = Message {
    tenant_key: text_at(event, &["header", TENANT_KEY])?.to_string(),
    chat_id: text_at(raw, &[CHAT_ID])?.to_string(),
    message_id: text_at(raw, &[MESSAGE_ID])?.to_string(),
    root_id: optional_text(raw, ROOT_ID)?,
    thread_id: optional_text(raw, THREAD_ID)?,
    create_time: text_at(raw, &[CREATE_TIME])?.to_string(),
    sender_open_id: sender_open_id.to_string(),
    mentions,
  };
  if mentioned {
    return Ok(Inbound::Summon(message));
  }
  if message.thread_id.is_none() {
    return Ok(Inbound::Drop(Drop::NoMentionOutsideThread));
  }
  Ok(Inbound::Follow(message))
}

impl Message {
  /// The root message id that names the session's thread: the thread's root when the message is in a thread, else the
  /// message itself. A summon finds its session through this id before the thread exists.
  pub fn thread_root(&self) -> anyhow::Result<&str> {
    if self.thread_id.is_none() {
      return Ok(&self.message_id);
    }
    match self.root_id.as_deref() {
      Some(root) => Ok(root),
      None => bail!("message {} is in thread {:?} but names no root", self.message_id, self.thread_id),
    }
  }
}

/// The mentions of one message object, in order (`mentions[]`). The event carries each mention's `id` as an object
/// with an `open_id`; the message list carries the open_id as the `id` string. A mention in neither shape fails.
pub fn mentions_of(message: &Value) -> anyhow::Result<Vec<Mention>> {
  let Some(list) = message.get("mentions").filter(|value| !value.is_null()) else {
    return Ok(Vec::new());
  };
  let list = list.as_array().context("the message's mentions are not a list")?;
  list
    .iter()
    .map(|mention| {
      let id = mention.get("id").context("a mention has no id")?;
      let open_id = match id {
        Value::String(open_id) => open_id.as_str(),
        Value::Object(_) => text_at(id, &["open_id"])?,
        _ => bail!("a mention's id is neither a string nor an object"),
      };
      Ok(Mention {
        key: text_at(mention, &["key"])?.to_string(),
        open_id: open_id.to_string(),
        name: mention.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
      })
    })
    .collect()
}

/// The text a message shows: a text message's `text` with each mention placeholder replaced by `@<name>`. Any other
/// message type shows its content as written, so the keys of its images and files stay readable.
pub fn render_text(message_type: &str, content: &str, mentions: &[Mention]) -> anyhow::Result<String> {
  if message_type != "text" {
    return Ok(content.to_string());
  }
  let parsed: Value = serde_json::from_str(content).context("a text message's content is not JSON")?;
  let mut text =
    parsed.get("text").and_then(Value::as_str).context("a text message's content has no text")?.to_string();
  for mention in mentions {
    text = text.replace(&mention.key, &format!("@{}", mention.name));
  }
  Ok(text)
}

/// The string at a path of object keys, or an error naming the missing key.
fn text_at<'a>(object: &'a Value, path: &[&str]) -> anyhow::Result<&'a str> {
  let mut value = object;
  for key in path {
    value = value.get(*key).with_context(|| format!("the event has no {}", path.join(".")))?;
  }
  value.as_str().with_context(|| format!("{} is not a string", path.join(".")))
}

/// A string field that may be absent or empty: absent and empty both read as None.
fn optional_text(object: &Value, key: &str) -> anyhow::Result<Option<String>> {
  match object.get(key) {
    None | Some(Value::Null) => Ok(None),
    Some(value) => {
      let text = value.as_str().with_context(|| format!("{key} is not a string"))?;
      Ok((!text.is_empty()).then(|| text.to_string()))
    }
  }
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::*;

  const BOT: &str = "ou_synthetic_bot";
  const HUMAN: &str = "ou_synthetic_user";

  /// A group message from `sender` with the given message fields merged over the defaults.
  fn event(sender_type: &str, sender: &str, chat_type: &str, extra: Value) -> Value {
    let mut message = json!({
      "message_id": "om_100",
      "chat_id": "oc_synthetic",
      "chat_type": chat_type,
      "create_time": "1700000000000",
      "message_type": "text",
      "content": "{\"text\":\"@_user_1 hello\"}",
      "mentions": [{"key": "@_user_1", "id": {"open_id": BOT}, "name": "CharlieBot"}],
    });
    if let (Value::Object(base), Value::Object(extra)) = (&mut message, extra) {
      base.extend(extra);
    }
    json!({
      "schema": "2.0",
      "header": {"event_id": "ev_1", "event_type": RECEIVE_EVENT, "tenant_key": "tk_synthetic"},
      "event": {
        "sender": {"sender_id": {"open_id": sender}, "sender_type": sender_type},
        "message": message,
      },
    })
  }

  #[test]
  fn a_mention_from_a_human_in_a_group_is_a_summon() {
    let inbound = classify(&event("user", HUMAN, "group", json!({})), BOT).unwrap();
    let Inbound::Summon(message) = inbound else { panic!("expected a summon, got {inbound:?}") };
    assert_eq!(message.message_id, "om_100");
    assert_eq!(message.thread_root().unwrap(), "om_100");
    assert_eq!(message.mentions[0].open_id, BOT);
  }

  #[test]
  fn a_thread_message_without_a_mention_from_a_human_is_a_follow() {
    let extra = json!({"thread_id": "omt_1", "root_id": "om_50", "mentions": []});
    let inbound = classify(&event("user", HUMAN, "group", extra), BOT).unwrap();
    let Inbound::Follow(message) = inbound else { panic!("expected a follow") };
    assert_eq!(message.thread_root().unwrap(), "om_50");
  }

  #[test]
  fn a_mention_inside_a_thread_summons_that_thread() {
    let extra = json!({"thread_id": "omt_1", "root_id": "om_50"});
    let inbound = classify(&event("user", HUMAN, "group", extra), BOT).unwrap();
    let Inbound::Summon(message) = inbound else { panic!("expected a summon") };
    assert_eq!(message.thread_root().unwrap(), "om_50");
  }

  #[test]
  fn a_mention_from_any_human_is_classified_without_a_list_of_senders() {
    let inbound = classify(&event("user", "ou_synthetic_stranger", "group", json!({})), BOT).unwrap();
    assert!(matches!(inbound, Inbound::Summon(_)), "got {inbound:?}");
  }

  #[test]
  fn a_one_to_one_message_is_dropped_even_from_a_human() {
    let inbound = classify(&event("user", HUMAN, "p2p", json!({})), BOT).unwrap();
    assert_eq!(inbound, Inbound::Drop(Drop::NotGroup));
  }

  #[test]
  fn the_bots_own_and_other_app_messages_are_dropped() {
    let inbound = classify(&event("app", BOT, "group", json!({})), BOT).unwrap();
    assert_eq!(inbound, Inbound::Drop(Drop::NotUser));
  }

  #[test]
  fn a_group_message_neither_mentioning_the_bot_nor_in_a_thread_is_dropped() {
    let extra = json!({"mentions": []});
    let inbound = classify(&event("user", HUMAN, "group", extra), BOT).unwrap();
    assert_eq!(inbound, Inbound::Drop(Drop::NoMentionOutsideThread));
  }

  #[test]
  fn another_event_type_is_dropped() {
    let mut other = event("user", HUMAN, "group", json!({}));
    other["header"]["event_type"] = json!("im.chat.member.bot.added_v1");
    assert_eq!(classify(&other, BOT).unwrap(), Inbound::Drop(Drop::OtherEvent));
  }

  #[test]
  fn an_event_without_its_sender_is_an_error() {
    let mut malformed = event("user", HUMAN, "group", json!({}));
    malformed["event"].as_object_mut().unwrap().remove("sender");
    assert!(classify(&malformed, BOT).is_err());
  }

  #[test]
  fn a_text_message_renders_its_mention_as_the_name() {
    let mentions = vec![Mention { key: "@_user_1".into(), open_id: BOT.into(), name: "CharlieBot".into() }];
    let text = render_text("text", "{\"text\":\"@_user_1 hello\"}", &mentions).unwrap();
    assert_eq!(text, "@CharlieBot hello");
  }

  #[test]
  fn an_image_message_renders_its_content_as_written() {
    let text = render_text("image", "{\"image_key\":\"img_v2_1\"}", &[]).unwrap();
    assert_eq!(text, "{\"image_key\":\"img_v2_1\"}");
  }
}
