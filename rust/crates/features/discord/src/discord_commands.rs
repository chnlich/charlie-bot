//! The Discord server-side commands behind `charliebot discord read` and
//! `charliebot discord check`. A read pages the thread through the bot client, prepends the thread's starter when it
//! lives in the parent channel, acks the unread messages it returns (Discord has no separate ack verb, so the read is
//! the ack), and flags the pending summon's mention with `summon: true`. A linked channel is read without any mark.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use axum::http::StatusCode;
use charliebot_chat_threads::thread_entry::{self, IdOrdering, ThreadAdapter, ThreadReplyError};
use charliebot_core::text_repr;
use serde_json::{Value, json};

use crate::config::DiscordConfig;
use crate::discord_client::{
  DiscordAPIError, Message, message_content_intent_enabled, missing_permissions, parse_message_link,
};
use crate::discord_listener::{Bot, DISCORD, DiscordThreadAdapter, eligible_message};
use crate::metadata::DiscordOrigin;

/// The readback's `message` view of one thread message: ids, the author's name and configured person,
/// the text, the attachment URLs, and the unread and summon flags.
fn message_view(message: &Message, unread: bool, summon: bool, allowed_users: &BTreeMap<String, String>) -> Value {
  json!({
    "id": message.id,
    "author_id": message.author.id,
    "author": message.author.global_name.as_deref().filter(|name| !name.is_empty()).unwrap_or(message.author.username.as_str()),
    "person": allowed_users.get(&message.author.id),
    "timestamp": message.timestamp,
    "content": message.content.as_deref().unwrap_or_default(),
    "attachments": message.attachments.iter().map(|attachment| attachment.url.as_str()).collect::<Vec<_>>(),
    "unread": unread,
    "summon": summon,
  })
}

/// The HTTP status a failed Discord call carries, or None when the call failed for another reason.
fn api_status(error: &anyhow::Error) -> Option<u16> {
  error.downcast_ref::<DiscordAPIError>().map(|api| api.status)
}

/// The refusal a failed Discord call maps to: 502 with the API error's text, or the unhandled failure (500) for any
/// other error.
fn discord_failure(error: anyhow::Error) -> ThreadReplyError {
  match error.downcast::<DiscordAPIError>() {
    Ok(api) => ThreadReplyError::refused(StatusCode::BAD_GATEWAY, api.to_string()),
    Err(other) => ThreadReplyError::failure(other),
  }
}

/// The session's pending summons' mention ids, each rendered as text.
async fn pending_summon_ids(bot: &Bot, session_id: &str) -> Result<HashSet<String>, ThreadReplyError> {
  let events = thread_entry::load_chat_events(&bot.features, session_id).await?;
  thread_entry::pending_summon_mention_ids(&DISCORD, &bot.features.store, session_id, &events, |mention| {
    Ok(text_repr::value_str(&mention.to_value()))
  })
  .map_err(ThreadReplyError::failure)
}

/// Read the session's own Discord thread, and ack the unread messages the window returns. The
/// window starts at the earlier of the oldest unread message and the oldest pending summon's mention, or holds the
/// newest `limit` messages when neither exists. The starter rides first, outside the count, when it lives in the parent
/// channel: a 404 there means the thread has none.
async fn read_own_thread(bot: &Bot, session_id: &str, limit: usize) -> Result<Value, ThreadReplyError> {
  let session = thread_entry::require_thread_session(&DISCORD, &bot.features, session_id).await?;
  let origin: DiscordOrigin = serde_json::from_value(session.origin.clone()).map_err(ThreadReplyError::failure)?;
  let watermark = session.meta.extra("discord_watermark_id").and_then(Value::as_str).map(str::to_string);
  let config = bot.config().map_err(ThreadReplyError::failure)?;
  let allowed_users = &config.section::<DiscordConfig>("discord").allowed_users;
  let adapter = Arc::new(DiscordThreadAdapter::new(bot.clone()));
  let client = adapter.client().await.map_err(ThreadReplyError::failure)?;
  let thread_messages = client.all_messages(&origin.thread_id).await.map_err(discord_failure)?;
  let starter = match client.get_message(&origin.parent_channel_id, &origin.thread_id).await {
    Ok(message) => Some(message),
    Err(error) if api_status(&error) == Some(404) => None,
    Err(error) => return Err(discord_failure(error)),
  };
  // One read feeds both the window and the unread flags: a message that lands after this fetch is simply not in the
  // readback, never an id the window pick misses.
  let unread_ids: HashSet<String> = thread_entry::unread_after(
    &thread_messages,
    |message| eligible_message(message, allowed_users),
    |message| message.id.as_str(),
    watermark.as_deref(),
    IdOrdering::Snowflake,
  )
  .into_iter()
  .map(|message| message.id.clone())
  .collect();
  let pending = pending_summon_ids(bot, session_id).await?;
  let mut starts = Vec::new();
  if let Some(first) = thread_messages.iter().position(|message| unread_ids.contains(&message.id)) {
    starts.push(first);
  }
  if let Some(first) = thread_messages.iter().position(|message| pending.contains(&message.id)) {
    starts.push(first);
  }
  let window: &[Message] = match starts.into_iter().min() {
    None => &thread_messages[thread_messages.len().saturating_sub(limit)..],
    Some(start) => &thread_messages[start..thread_messages.len().min(start + limit)],
  };
  let window_unread_ids: Vec<String> =
    window.iter().filter(|message| unread_ids.contains(&message.id)).map(|message| message.id.clone()).collect();
  let mut watermark_id = json!(watermark);
  if !window_unread_ids.is_empty() {
    let dyn_adapter: Arc<dyn ThreadAdapter> = adapter.clone();
    let ack = thread_entry::ack_messages(&dyn_adapter, &bot.features, session_id, &window_unread_ids).await?;
    watermark_id = ack["watermark_id"].clone();
  }
  let mut messages = Vec::new();
  if let Some(starter) = &starter {
    messages.push(message_view(starter, false, pending.contains(&starter.id), allowed_users));
  }
  messages.extend(window.iter().map(|message| {
    message_view(message, unread_ids.contains(&message.id), pending.contains(&message.id), allowed_users)
  }));
  Ok(json!({
    "messages": messages,
    "watermark_id": watermark_id,
    "more_unread": unread_ids.len() - window_unread_ids.len(),
  }))
}

/// Read the newest `limit` messages of the channel a discord.com link names; nothing is marked read.
/// A link that is not one refuses with 422, a channel the bot cannot see with 404.
async fn read_linked_channel(bot: &Bot, url: &str, limit: usize) -> Result<Value, ThreadReplyError> {
  let link = parse_message_link(url)
    .map_err(|error| ThreadReplyError::refused(StatusCode::UNPROCESSABLE_ENTITY, error.to_string()))?;
  let client = bot.client().map_err(ThreadReplyError::failure)?;
  let newest = match client.get_messages(&link.channel_id, None, limit).await {
    Ok(newest) => newest,
    Err(error) if matches!(api_status(&error), Some(403 | 404)) => {
      return Err(ThreadReplyError::refused(StatusCode::NOT_FOUND, "the bot cannot see this channel"));
    }
    Err(error) => return Err(discord_failure(error)),
  };
  let config = bot.config().map_err(ThreadReplyError::failure)?;
  let allowed_users = &config.section::<DiscordConfig>("discord").allowed_users;
  let messages: Vec<Value> = newest.iter().map(|message| message_view(message, false, false, allowed_users)).collect();
  Ok(json!({"messages": messages, "watermark_id": null, "more_unread": 0}))
}

/// The readback of `charliebot discord read`: the session's own thread (`url` None), or the newest messages of the
/// channel a link names. Refusals: 404 unknown session, 409 no Discord thread, 422 a link that is not discord.com, 404
/// a channel the bot cannot see, 502 Discord refused the read.
pub async fn read_thread(
  bot: &Bot,
  session_id: &str,
  url: Option<&str>,
  limit: usize,
) -> Result<Value, ThreadReplyError> {
  match url {
    Some(url) => read_linked_channel(bot, url, limit).await,
    None => read_own_thread(bot, session_id, limit).await,
  }
}

/// The readback of `charliebot discord check`: the bot user, the message-content intent, and each guild's missing
/// permissions. `ok` is the intent on and no guild missing anything. Refusals: 409 when the bot token is unset, 502 when
/// a Discord call fails.
pub async fn check_setup(bot: &Bot) -> Result<Value, ThreadReplyError> {
  let credentials = bot.features.credentials.get().map_err(ThreadReplyError::failure)?;
  if credentials.get("discord", "bot_token").is_none() {
    return Err(ThreadReplyError::refused(StatusCode::CONFLICT, "credentials.discord.bot_token is not set"));
  }
  let client = bot.client().map_err(ThreadReplyError::failure)?;
  let user = client.get_current_user().await.map_err(discord_failure)?;
  let application = client.get_current_application().await.map_err(discord_failure)?;
  let guilds = client.list_current_user_guilds().await.map_err(discord_failure)?;
  let intent = message_content_intent_enabled(application.flags);
  let mut guild_views = Vec::with_capacity(guilds.len());
  let mut any_missing = false;
  for guild in &guilds {
    let permissions: u64 = guild.permissions.parse().map_err(ThreadReplyError::failure)?;
    let missing = missing_permissions(permissions);
    any_missing |= !missing.is_empty();
    guild_views.push(json!({"id": guild.id, "name": guild.name, "missing_permissions": missing}));
  }
  Ok(json!({
    "ok": intent && !any_missing,
    "bot_user": {"id": user.id, "username": user.username},
    "application_id": application.id,
    "message_content_intent": intent,
    "guilds": guild_views,
  }))
}

#[cfg(test)]
mod tests;
