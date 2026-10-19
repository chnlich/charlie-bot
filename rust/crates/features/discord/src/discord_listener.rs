//! The Discord side of the shared thread entrypoint (`src/features/discord/discord_listener.py`): the `DISCORD` platform,
//! the summon session id, the eligibility rule, the `Bot` handle that builds the REST client on the bot token, the
//! `DiscordThreadAdapter` the shared thread core works through, and the round-side wrappers the reply, read and ack
//! routes call. The gateway loop and `handle_message_create` are not ported yet.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Context;
use charliebot_chat_threads::thread_entry::{self, IdOrdering, ThreadAdapter, ThreadMessage, ThreadPlatform};
use charliebot_compat::http_client;
use charliebot_core::config::CharlieBotConfig;
use charliebot_registry::FeatureContext;
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use tokio::sync::OnceCell;
use uuid::Uuid;

use crate::config::DiscordConfig;
use crate::discord_client::{DiscordClient, Message};
use crate::event_types::DISCORD_REPLY;
use crate::metadata::DiscordOrigin;

/// The namespace of Discord summon session ids. Changing it would orphan every existing Discord-backed session.
const DISCORD_NS: Uuid = Uuid::from_u128(0x13667dae_7551_489f_9cf1_d0819d0978e6);

/// The 👀 reaction: the summon ack, lit at the summon and cleared when the question closes.
const ACK_EMOJI: &str = "👀";

/// Discord's per-message text limit, which is also the chunking target.
pub const MAX_POST_CHARS: usize = 2000;

/// The command the thread reply contract names for posting a reply.
pub const REPLY_COMMAND: &str = "charliebot discord reply";

/// The command the contract names for reading the thread server-side.
pub const READ_COMMAND: &str = "charliebot discord read";

/// The trigger-label prefix of a session's armed thread-follow record.
pub const FOLLOW_TRIGGER_PREFIX: &str = "discord-thread-follow";

/// The platform description every shared thread entry takes (`discord_listener.DISCORD`).
pub const DISCORD: ThreadPlatform = ThreadPlatform {
  name: "discord",
  display_name: "Discord",
  reply_event_type: DISCORD_REPLY,
  reply_command: REPLY_COMMAND,
  max_post_chars: MAX_POST_CHARS,
  scope_doc: "discord_reply_scope.md",
  follow_trigger_prefix: FOLLOW_TRIGGER_PREFIX,
  id_key: IdOrdering::Snowflake,
  origin_field: "discord_origin",
  watermark_field: "discord_watermark_id",
  id_label: "id",
  mention_key: "mention_id",
  block_keys: &["guild_id", "channel_id", "thread_id", "mention_id"],
  summon_line: "Discord 线程召唤",
};

/// The deterministic session id of the Discord thread `thread_id` in `guild_id` (`summon_session_id`).
pub fn summon_session_id(guild_id: &str, thread_id: &str) -> String {
  Uuid::new_v5(&DISCORD_NS, format!("discord:{guild_id}:{thread_id}").as_bytes()).to_string()
}

/// Whether a message counts for the thread: a human's plain message or reply, from an account in `allowed_users`
/// (`eligible_message`). The follow path and the readback apply the same rule.
pub fn eligible_message(message: &Message, allowed_users: &BTreeMap<String, String>) -> bool {
  !message.author.bot
    && message.webhook_id.is_none()
    && matches!(message.kind, Some(0 | 19))
    && allowed_users.contains_key(&message.author.id)
}

/// The bot's handle on Discord: the feature context that reads the config and the credentials on each call, and the
/// REST base URL (`BASE_URL` in production).
#[derive(Clone)]
pub struct Bot {
  pub(crate) features: Arc<FeatureContext>,
  base_url: String,
}

impl Bot {
  pub fn new(features: Arc<FeatureContext>, base_url: impl Into<String>) -> Self {
    Self { features, base_url: base_url.into() }
  }

  /// The client every outbound call posts through, built on `credentials.discord.bot_token` (`_bot_client`). A missing
  /// token fails the call.
  pub fn client(&self) -> anyhow::Result<DiscordClient> {
    let credentials = self.features.credentials.get()?;
    let token = credentials.require("discord", "bot_token")?;
    Ok(DiscordClient::new(http_client::shared_client()?, self.base_url.clone(), token))
  }

  /// The loaded config, read for one call.
  pub(crate) fn config(&self) -> anyhow::Result<Arc<CharlieBotConfig>> {
    Ok(self.features.config.get()?)
  }
}

/// The Discord thread's `ThreadAdapter` (`DiscordThreadAdapter`). The client is built on the first platform call and
/// reused, so a refusal that never reaches Discord (a 404 or a 409 from the session check) needs no token.
pub struct DiscordThreadAdapter {
  bot: Bot,
  client: OnceCell<DiscordClient>,
}

impl DiscordThreadAdapter {
  pub fn new(bot: Bot) -> Self {
    Self { bot, client: OnceCell::new() }
  }

  /// The client every platform call posts through, built once.
  pub async fn client(&self) -> anyhow::Result<&DiscordClient> {
    self.client.get_or_try_init(|| async { self.bot.client() }).await
  }
}

impl ThreadAdapter for DiscordThreadAdapter {
  fn platform(&self) -> &ThreadPlatform {
    &DISCORD
  }

  fn post<'a>(&'a self, address: &'a Value, text: &'a str) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move {
      self.client().await?.create_message(str_field(address, "thread_id")?, text).await?;
      Ok(())
    })
  }

  fn add_ack<'a>(&'a self, block: &'a Value) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move {
      let channel = str_field(block, "channel_id")?;
      let message = str_field(block, DISCORD.mention_key)?;
      self.client().await?.add_reaction(channel, message, ACK_EMOJI).await
    })
  }

  fn remove_ack<'a>(&'a self, block: &'a Value) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move {
      let channel = str_field(block, "channel_id")?;
      let message = str_field(block, DISCORD.mention_key)?;
      self.client().await?.remove_own_reaction(channel, message, ACK_EMOJI).await
    })
  }

  fn read_eligible<'a>(
    &'a self,
    origin: &'a Value,
    config: &'a CharlieBotConfig,
  ) -> BoxFuture<'a, anyhow::Result<Vec<ThreadMessage>>> {
    Box::pin(async move {
      let origin = origin_of(origin)?;
      let allowed_users = &config.section::<DiscordConfig>("discord").allowed_users;
      let messages = self.client().await?.all_messages(&origin.thread_id).await?;
      Ok(
        messages
          .iter()
          .filter(|message| eligible_message(message, allowed_users))
          .map(|message| ThreadMessage {
            id: message.id.clone(),
            user: Some(message.author.id.clone()),
            text: message.content.clone().unwrap_or_default(),
          })
          .collect(),
      )
    })
  }

  fn address_of(&self, origin: &Value) -> Value {
    let origin = origin_of(origin).expect("a stored discord_origin names its guild, parent channel and thread");
    json!({"guild_id": origin.guild_id, "thread_id": origin.thread_id})
  }

  fn log_fields(&self, address: &Value) -> Value {
    json!({"guild": address["guild_id"], "thread": address["thread_id"]})
  }
}

/// The session's origin as the adapter reads it. The slot validator has checked its fields on every write.
fn origin_of(origin: &Value) -> anyhow::Result<DiscordOrigin> {
  serde_json::from_value(origin.clone()).context("discord_origin is not a Discord origin")
}

/// A string field of a stored block or address.
fn str_field<'a>(object: &'a Value, name: &str) -> anyhow::Result<&'a str> {
  object.get(name).and_then(Value::as_str).with_context(|| format!("the Discord block names no {name}"))
}

/// The adapter the wrappers hand the shared core.
fn adapter(bot: &Bot) -> Arc<dyn ThreadAdapter> {
  Arc::new(DiscordThreadAdapter::new(bot.clone()))
}

/// Refuse the reply while eligible thread messages sit above the session's watermark (`assert_thread_fresh`).
pub async fn assert_thread_fresh(bot: &Bot, session_id: &str) -> Result<(), thread_entry::ThreadReplyError> {
  thread_entry::assert_thread_fresh(&adapter(bot), &bot.features, session_id).await
}

/// Advance the session's read watermark over `message_ids` and return the readback (`ack_messages`).
pub async fn ack_messages(
  bot: &Bot,
  session_id: &str,
  message_ids: &[String],
) -> Result<Value, thread_entry::ThreadReplyError> {
  thread_entry::ack_messages(&adapter(bot), &bot.features, session_id, message_ids).await
}

/// Post `text` to the session's Discord thread and return the readback the CLI prints (`post_reply`).
pub async fn post_reply(bot: &Bot, session_id: &str, text: &str) -> Result<Value, thread_entry::ThreadReplyError> {
  thread_entry::post_reply(&adapter(bot), &bot.features, session_id, text).await
}
