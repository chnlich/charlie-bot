//! The Discord server-side commands behind `charliebot discord read` and
//! `charliebot discord check`. A read pages the thread through the bot client. It prepends the
//! thread's starter when it lives in the parent channel. It acks the unread messages it returns:
//! Discord has no separate ack verb, so the read is the ack. It flags the pending summon's mention
//! with `summon: true`. A linked channel is read without any mark.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use axum::http::StatusCode;
use charliebot_chat_threads::thread_entry::{self, IdOrdering, ThreadReplyError};
use charliebot_chat_threads::thread_read;
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
    (thread_read::ID): message.id,
    (thread_read::AUTHOR_ID): message.author.id,
    "author": message.author.global_name.as_deref().filter(|name| !name.is_empty()).unwrap_or(message.author.username.as_str()),
    (thread_read::PERSON): allowed_users.get(&message.author.id),
    (thread_read::TIMESTAMP): message.timestamp,
    (thread_read::CONTENT): message.content.as_deref().unwrap_or_default(),
    "attachments": message.attachments.iter().map(|attachment| attachment.url.as_str()).collect::<Vec<_>>(),
    (thread_read::UNREAD): unread,
    (thread_read::SUMMON): summon,
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

impl Bot {
  /// The session's pending summons' mention ids, each rendered as text.
  async fn pending_summon_ids(&self, session_id: &str) -> Result<HashSet<String>, ThreadReplyError> {
    let events = thread_entry::load_chat_events(&self.features, session_id).await?;
    thread_entry::pending_summon_mention_ids(&DISCORD, &self.features.store, session_id, &events, |mention| {
      Ok(text_repr::value_str(&mention.to_value()))
    })
    .map_err(ThreadReplyError::failure)
  }

  /// The readback of the session's own Discord thread, with the unread messages the window returns acked. The
  /// window follows the shared thread-read rule: a pending summon's mention opens it only when that mention and the
  /// oldest unread message fit inside one `limit`-sized window. The starter rides first, outside the count, when it
  /// lives in the parent channel: a 404 there means the thread has none.
  async fn read_own_thread(&self, session_id: &str, limit: usize) -> Result<Value, ThreadReplyError> {
    let session = thread_entry::require_thread_session(&DISCORD, &self.features, session_id).await?;
    let origin: DiscordOrigin = serde_json::from_value(session.origin.clone()).map_err(ThreadReplyError::failure)?;
    let watermark = session.meta.extra(DISCORD.watermark_field).and_then(Value::as_str).map(str::to_string);
    let config = self.config().map_err(ThreadReplyError::failure)?;
    let allowed_users = &config.section::<DiscordConfig>("discord").allowed_users;
    let adapter = Arc::new(DiscordThreadAdapter::new(self.clone()));
    let client = adapter.client().await.map_err(ThreadReplyError::failure)?;
    let thread_messages = client.all_messages(&origin.thread_id).await.map_err(discord_failure)?;
    let starter = match client.get_message(&origin.parent_channel_id, &origin.thread_id).await {
      Ok(message) => Some(message),
      Err(error) if api_status(&error) == Some(404) => None,
      Err(error) => return Err(discord_failure(error)),
    };
    // One read feeds both the window and the unread flags. A message that lands after this fetch is simply not in the
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
    let pending = self.pending_summon_ids(session_id).await?;
    let window = thread_read::read_window(
      &adapter,
      &self.features,
      session_id,
      thread_read::ThreadRead {
        messages: &thread_messages,
        unread_ids: &unread_ids,
        pending: &pending,
        watermark: watermark.as_deref(),
        limit,
      },
      |message| message.id.as_str(),
    )
    .await?;
    let mut messages = Vec::new();
    if let Some(starter) = &starter {
      messages.push(message_view(starter, false, pending.contains(&starter.id), allowed_users));
    }
    messages.extend(thread_messages[window.first..window.last].iter().map(|message| {
      message_view(message, unread_ids.contains(&message.id), pending.contains(&message.id), allowed_users)
    }));
    Ok(json!({
      (thread_read::MESSAGES): messages,
      (DISCORD.watermark_key()): window.watermark,
      (thread_read::MORE_UNREAD): window.more_unread,
    }))
  }

  /// The readback of the newest `limit` messages of the channel a discord.com link names. Nothing is marked read.
  /// A link that is not one refuses with 422, a channel the bot cannot see with 404.
  async fn read_linked_channel(&self, url: &str, limit: usize) -> Result<Value, ThreadReplyError> {
    let link = parse_message_link(url)
      .map_err(|error| ThreadReplyError::refused(StatusCode::UNPROCESSABLE_ENTITY, error.to_string()))?;
    let client = self.client().map_err(ThreadReplyError::failure)?;
    let newest = match client.get_messages(&link.channel_id, None, limit).await {
      Ok(newest) => newest,
      Err(error) if matches!(api_status(&error), Some(403 | 404)) => {
        return Err(ThreadReplyError::refused(StatusCode::NOT_FOUND, "the bot cannot see this channel"));
      }
      Err(error) => return Err(discord_failure(error)),
    };
    let config = self.config().map_err(ThreadReplyError::failure)?;
    let allowed_users = &config.section::<DiscordConfig>("discord").allowed_users;
    let messages: Vec<Value> =
      newest.iter().map(|message| message_view(message, false, false, allowed_users)).collect();
    Ok(json!({(thread_read::MESSAGES): messages, (DISCORD.watermark_key()): null, (thread_read::MORE_UNREAD): 0}))
  }

  /// The readback of `charliebot discord read`: the session's own thread (`url` None), or the newest messages of the
  /// channel a link names. Refusals: 404 unknown session, 409 no Discord thread, 422 a link that is not discord.com,
  /// 404 a channel the bot cannot see, 502 Discord refused the read.
  pub async fn read_thread(
    &self,
    session_id: &str,
    url: Option<&str>,
    limit: usize,
  ) -> Result<Value, ThreadReplyError> {
    match url {
      Some(url) => self.read_linked_channel(url, limit).await,
      None => self.read_own_thread(session_id, limit).await,
    }
  }

  /// The readback of `charliebot discord check`: the bot user, the message-content intent, and each guild's missing
  /// permissions. `ok` is the intent on and no guild missing anything. Refusals: 409 when the bot token is unset, 502
  /// when a Discord call fails.
  pub async fn check_setup(&self) -> Result<Value, ThreadReplyError> {
    let credentials = self.features.credentials.get().map_err(ThreadReplyError::failure)?;
    if crate::config::bot_token(&credentials).is_none() {
      return Err(ThreadReplyError::refused(StatusCode::CONFLICT, "credentials.discord.bot_token is not set"));
    }
    let client = self.client().map_err(ThreadReplyError::failure)?;
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
}

#[cfg(test)]
mod tests;
