//! The Discord side of the shared thread entrypoint (`src/features/discord/discord_listener.py`): the gateway listener,
//! summon handler, `DISCORD` platform and `DiscordThreadAdapter` the shared thread core works through.

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use anyhow::Context;
use charliebot_chat_threads::channel_service::ChannelListener;
use charliebot_chat_threads::thread_entry::{self, Summon, ThreadAdapter, ThreadMessage, ThreadPlatform};
use charliebot_compat::http_client;
use charliebot_compat::ws_client::{self, CloseFrame, Message as WsMessage, Options, Receiver, Sender};
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::pystr;
use charliebot_registry::FeatureContext;
use charliebot_store::metadata::UtcDateTime;
use charliebot_triggers::triggers::TriggerManager;
use futures_util::future::BoxFuture;
use regex::Regex;
use serde_json::{Value, json};
use tokio::sync::OnceCell;
use tokio::time::Instant;
use uuid::Uuid;

use crate::config::DiscordConfig;
use crate::discord_client::{DiscordClient, Message};
use crate::metadata::DiscordOrigin;
pub use crate::thread_contract::{DISCORD, FOLLOW_TRIGGER_PREFIX, MAX_POST_CHARS, READ_COMMAND, REPLY_COMMAND};

/// The namespace of Discord summon session ids. Changing it would orphan every existing Discord-backed session.
const DISCORD_NS: Uuid = Uuid::from_u128(0x13667dae_7551_489f_9cf1_d0819d0978e6);

/// The 👀 reaction: the summon ack, lit at the summon and cleared when the question closes.
const ACK_EMOJI: &str = "👀";
const INTENTS: u64 = (1 << 0) | (1 << 9) | (1 << 12) | (1 << 15);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
const DM_NOTICE: &str = "请在服务器频道里 @ 我。";
const THREAD_NAME_CHARS: usize = 80;
const STOP_CLOSE_CODES: &[(u16, &str)] = &[
  (4004, "authentication failed"),
  (4010, "invalid shard"),
  (4011, "sharding required"),
  (4012, "invalid API version"),
  (4013, "invalid intents"),
  (4014, "disallowed intents: enable the Message Content intent in the Discord developer portal"),
];
static MENTION_TOKEN_RE: LazyLock<Regex> =
  LazyLock::new(|| Regex::new(r"<@[!&]?\d+>").expect("the Discord mention pattern is valid"));

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
    Ok(DiscordClient::new(http_client::client(&http_client::Settings::from_env())?, self.base_url.clone(), token))
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
  bot_user_id: OnceCell<String>,
}

impl DiscordThreadAdapter {
  pub fn new(bot: Bot) -> Self {
    Self { bot, client: OnceCell::new(), bot_user_id: OnceCell::new() }
  }

  fn with_client(bot: Bot, client: DiscordClient) -> Self {
    let cell = OnceCell::new();
    assert!(cell.set(client).is_ok(), "the Discord REST client is inserted into an empty slot");
    Self { bot, client: cell, bot_user_id: OnceCell::new() }
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

  fn post<'a>(&'a self, address: &'a Value, text: &'a str, post_key: &'a str) -> BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move {
      self.client().await?.create_message_with_post_key(str_field(address, "thread_id")?, text, post_key).await?;
      Ok(())
    })
  }

  fn find_own_post_by_key<'a>(
    &'a self,
    address: &'a Value,
    _post_key: &'a str,
    text: &'a str,
    created_after: &'a str,
    messages: Option<&'a [ThreadMessage]>,
  ) -> BoxFuture<'a, anyhow::Result<bool>> {
    Box::pin(async move {
      let client = self.client().await?;
      let bot_user_id = self
        .bot_user_id
        .get_or_try_init(|| async { Ok::<_, anyhow::Error>(client.get_current_user().await?.id) })
        .await?;
      let created_after = UtcDateTime::parse(created_after)?;
      let thread = match messages {
        Some(messages) => messages.to_vec(),
        None => client
          .all_messages(str_field(address, "thread_id")?)
          .await?
          .into_iter()
          .map(|message| ThreadMessage {
            id: message.id,
            user: Some(message.author.id),
            text: message.content.unwrap_or_default(),
            created_at: Some(message.timestamp),
            metadata: None,
            bot_id: None,
            is_bot: message.author.bot,
            eligible: false,
          })
          .collect(),
      };
      for message in thread {
        if message.user.as_deref() == Some(bot_user_id.as_str()) && message.text == text {
          let timestamp = message.created_at.as_deref().context("Discord thread message has no timestamp")?;
          if UtcDateTime::parse(timestamp)? >= created_after {
            return Ok(true);
          }
        }
      }
      Ok(false)
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

  fn read_thread<'a>(
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
          .map(|message| ThreadMessage {
            id: message.id.clone(),
            user: Some(message.author.id.clone()),
            text: message.content.clone().unwrap_or_default(),
            created_at: Some(message.timestamp.clone()),
            metadata: None,
            bot_id: None,
            is_bot: message.author.bot,
            eligible: eligible_message(message, allowed_users),
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
  post_reply_with_caller_context(bot, session_id, text, None, None).await
}

/// Post a reply bound to the verified caller's run request.
pub async fn post_reply_with_caller_context(
  bot: &Bot,
  session_id: &str,
  text: &str,
  request_id: Option<&str>,
  run_started_at: Option<&str>,
) -> Result<Value, thread_entry::ThreadReplyError> {
  thread_entry::post_reply_with_caller_context(
    &adapter(bot),
    &bot.features,
    session_id,
    text,
    request_id,
    run_started_at,
  )
  .await
}

/// The listener implementation passed to the shared `ChannelService` (`run_listener`).
pub(crate) struct DiscordListener;

impl ChannelListener for DiscordListener {
  fn run_listener(&self, features: Arc<FeatureContext>) -> BoxFuture<'static, anyhow::Result<()>> {
    Box::pin(async move { run_listener(features).await })
  }

  fn backfill_lost_summons(&self, features: Arc<FeatureContext>) -> BoxFuture<'static, anyhow::Result<usize>> {
    Box::pin(async move { backfill_lost_summons(&features).await })
  }
}

/// Accept or drop one gateway `MESSAGE_CREATE` payload (`handle_message_create`).
pub async fn handle_message_create(
  message: &Value,
  features: &Arc<FeatureContext>,
  client: Arc<DiscordClient>,
  bot_user_id: &str,
) -> anyhow::Result<Option<String>> {
  let config = features.config.get()?;
  let allowed_users = &config.section::<DiscordConfig>("discord").allowed_users;
  if !eligible_gateway_message(message, allowed_users) {
    return Ok(None);
  }

  let channel_id = required_text(message, "channel_id")?;
  let message_id = required_text(message, "id")?;
  let author_id = message.get("author").and_then(|author| author.get("id")).and_then(Value::as_str);
  let mentioned = mention_ids(message).contains(&bot_user_id);
  let guild_id = message.get("guild_id").and_then(Value::as_str);

  let Some(guild_id) = guild_id else {
    if mentioned {
      client.create_message_with_post_key(channel_id, DM_NOTICE, &format!("discord-dm:{message_id}#1")).await?;
      // The eligibility check admits only an author listed in `allowed_users`, so the id is present. The log
      // consumer matches the bare id, not the `Option` debug form.
      let author = author_id.context("an eligible gateway message carries its author id")?;
      tracing::info!(event = "discord_dm_notice_posted", channel = channel_id, user = author);
    }
    return Ok(None);
  };

  let bot = Bot::new(Arc::clone(features), crate::discord_client::BASE_URL);
  let adapter: Arc<dyn ThreadAdapter> = Arc::new(DiscordThreadAdapter::with_client(bot, client.as_ref().clone()));
  if !mentioned {
    return Ok(
      thread_entry::follow_message(
        &adapter,
        features,
        &summon_session_id(guild_id, channel_id),
        message_id,
        |origin| origin.get("thread_id").and_then(Value::as_str) == Some(channel_id),
      )
      .await?,
    );
  }

  let channel = client.get_channel(channel_id).await?;
  let (thread_id, parent_channel_id, parent) = match channel.kind {
    10..=12 => {
      let parent_channel_id = channel.parent_id.as_deref().context("a Discord thread channel has no parent_id")?;
      (channel_id.to_string(), parent_channel_id.to_string(), client.get_channel(parent_channel_id).await?)
    }
    0 | 5 => {
      let content = required_text(message, "content")?;
      let thread = client.start_thread_from_message(channel_id, message_id, &thread_name(content)).await?;
      (thread.id, channel_id.to_string(), channel)
    }
    channel_type => {
      tracing::info!(
        event = "discord_mention_dropped_channel_type",
        guild = guild_id,
        channel = channel_id,
        channel_type,
      );
      return Ok(None);
    }
  };

  let parent_name = parent.name.as_deref().context("a Discord parent channel has no name")?;
  let session_id = summon_session_id(guild_id, &thread_id);
  let origin = json!({
    "guild_id": guild_id,
    "parent_channel_id": parent_channel_id,
    "thread_id": thread_id,
  });
  let block = json!({
    "guild_id": guild_id,
    "channel_id": channel_id,
    "thread_id": thread_id,
    "mention_id": message_id,
  });
  Ok(Some(
    thread_entry::accept_summon(
      &adapter,
      features,
      Summon {
        session_id: &session_id,
        label: &format!("Discord #{parent_name}"),
        origin: &origin,
        block: &block,
        content: DISCORD.summon_line,
        user: author_id,
      },
    )
    .await?,
  ))
}

/// Whether a raw gateway message is a human-authored plain message or reply from an allowed user.
fn eligible_gateway_message(message: &Value, allowed_users: &BTreeMap<String, String>) -> bool {
  let author = message.get("author").filter(|value| !value.is_null());
  let bot =
    author.and_then(|author| author.get("bot")).is_some_and(|value| !value.is_null() && value != &Value::Bool(false));
  let webhook = message.get("webhook_id").is_some_and(|value| !value.is_null());
  let kind = message.get("type").and_then(Value::as_i64);
  let user = author.and_then(|author| author.get("id")).and_then(Value::as_str);
  !bot && !webhook && matches!(kind, Some(0 | 19)) && user.is_some_and(|user| allowed_users.contains_key(user))
}

/// The user ids in the message's `mentions` objects (`_mention_ids`).
fn mention_ids(message: &Value) -> Vec<&str> {
  message
    .get("mentions")
    .and_then(Value::as_array)
    .into_iter()
    .flatten()
    .filter_map(|mention| mention.get("id").and_then(Value::as_str))
    .collect()
}

/// The thread name made from the mention's content (`_thread_name`).
fn thread_name(content: &str) -> String {
  let stripped = MENTION_TOKEN_RE.replace_all(content, "");
  let name: String = pystr::collapse_whitespace(&stripped).chars().take(THREAD_NAME_CHARS).collect();
  if name.is_empty() { "CharlieBot".to_string() } else { name }
}

/// A string field on one gateway payload.
fn required_text<'a>(message: &'a Value, field: &str) -> anyhow::Result<&'a str> {
  message.get(field).and_then(Value::as_str).with_context(|| format!("Discord message has no string {field}"))
}

/// The round-end audit for one `master_done` (`deliver_done`).
pub async fn deliver_done(features: &Arc<FeatureContext>, session_id: &str, done: &Value) -> anyhow::Result<bool> {
  let bot = Bot::new(Arc::clone(features), crate::discord_client::BASE_URL);
  Ok(thread_entry::deliver_done(&adapter(&bot), features, session_id, done).await?)
}

/// Report queued Discord summons lost across a restart (`backfill_lost_summons`).
pub async fn backfill_lost_summons(features: &Arc<FeatureContext>) -> anyhow::Result<usize> {
  let bot = Bot::new(Arc::clone(features), crate::discord_client::BASE_URL);
  Ok(thread_entry::backfill_lost_summons(&adapter(&bot), features).await?)
}

/// Check the gateway intent and per-guild permissions once for a service start (`_preflight`).
async fn preflight(client: &DiscordClient) -> anyhow::Result<bool> {
  let application = client.get_current_application().await?;
  if !crate::discord_client::message_content_intent_enabled(application.flags) {
    tracing::error!(event = "discord_listener_message_content_intent_off");
    return Ok(false);
  }
  for guild in client.list_current_user_guilds().await? {
    let permissions =
      guild.permissions.parse::<u64>().context("Discord guild permissions are not an unsigned integer")?;
    let missing = crate::discord_client::missing_permissions(permissions);
    if !missing.is_empty() {
      tracing::error!(event = "discord_listener_missing_permissions", guild = guild.id, missing = ?missing);
    }
  }
  Ok(true)
}

/// Run the Discord gateway listener until a stop close code arrives (`run_listener`).
async fn run_listener(features: Arc<FeatureContext>) -> anyhow::Result<()> {
  let bot = Bot::new(Arc::clone(&features), crate::discord_client::BASE_URL);
  let client = Arc::new(bot.client()?);
  let _trigger_manager = features.startup.require::<Arc<TriggerManager>>()?;
  let token = features.credentials.get()?.require("discord", "bot_token")?;
  let options = Options { max_size: None, close_timeout: Some(CLOSE_TIMEOUT), ..ws_client::WEBSOCKETS_16_DEFAULTS };
  run_listener_loop(features, client, token, options, ws_client::Settings::from_env()).await
}

async fn run_listener_loop(
  features: Arc<FeatureContext>,
  client: Arc<DiscordClient>,
  token: String,
  options: Options,
  settings: ws_client::Settings,
) -> anyhow::Result<()> {
  if !preflight(&client).await? {
    return Ok(());
  }
  let mut backoff = Duration::from_secs(1);
  loop {
    let url = match client.get_gateway_url().await {
      Ok(url) => url,
      Err(error) => {
        tracing::warn!(event = "discord_listener_connect_failed", error = %error);
        tokio::time::sleep(backoff).await;
        backoff = next_backoff(backoff);
        continue;
      }
    };
    let url = format!("{url}/?v=10&encoding=json");
    let (sender, receiver) = match ws_client::connect(&url, &options, &settings).await {
      Ok(connection) => connection,
      Err(error) => {
        tracing::warn!(event = "discord_listener_connect_failed", error = %error);
        tokio::time::sleep(backoff).await;
        backoff = next_backoff(backoff);
        continue;
      }
    };

    let result = run_connection(&sender, receiver, &token, &features, Arc::clone(&client)).await;
    let close = sender.close().await;
    let result = match (result, close) {
      (Err(error), Err(close_error)) if !is_closed(&close_error) => {
        tracing::warn!(event = "discord_listener_connection_close_failed", error = %close_error);
        Err(error)
      }
      (Err(error), _) => Err(error),
      (Ok(_exit), Err(error)) if !is_closed(&error) => Err(error.into()),
      (Ok(exit), _) => Ok(exit),
    };

    match result {
      Ok(ConnectionExit::Stopped { code }) => {
        tracing::error!(event = "discord_listener_stopped", code, reason = stop_reason(code));
        return Ok(());
      }
      Ok(ConnectionExit::Dropped { error, saw_ready }) => {
        tracing::warn!(event = "discord_listener_connection_dropped", error);
        if saw_ready {
          backoff = Duration::from_secs(1);
        }
      }
      Err(error) => tracing::warn!(event = "discord_listener_connection_dropped", error = %error),
    }
    tokio::time::sleep(backoff).await;
    backoff = next_backoff(backoff);
  }
}

#[derive(Debug)]
enum ConnectionExit {
  Stopped { code: u16 },
  Dropped { error: String, saw_ready: bool },
}

async fn run_connection(
  sender: &Sender,
  mut receiver: Receiver,
  token: &str,
  features: &Arc<FeatureContext>,
  client: Arc<DiscordClient>,
) -> anyhow::Result<ConnectionExit> {
  let hello = match receive_json(&mut receiver, false).await? {
    Ok(hello) => hello,
    Err(exit) => return Ok(exit),
  };
  if hello.get("op").and_then(Value::as_u64) != Some(10) {
    tracing::warn!(event = "discord_listener_expected_hello", received = ?hello.get("op"));
  }
  let interval_ms = hello
    .get("d")
    .and_then(|data| data.get("heartbeat_interval"))
    .and_then(Value::as_u64)
    .context("Discord HELLO has no integer heartbeat_interval")?;
  let interval = Duration::from_millis(interval_ms);
  let mut next_heartbeat = Instant::now() + heartbeat_delay(interval)?;
  let mut last_sequence = Value::Null;
  let mut acked = true;
  let mut saw_ready = false;
  let mut bot_user_id = String::new();

  if let Err(error) = sender.send(WsMessage::Text(identify_payload(token).to_string())).await {
    return closed_exit(error, saw_ready);
  }

  loop {
    tokio::select! {
      _ = tokio::time::sleep_until(next_heartbeat) => {
        if !acked {
          let close = WsMessage::Close(Some(CloseFrame { code: 4000, reason: String::new() }));
          if let Err(error) = sender.send(close).await {
            return closed_exit(error, saw_ready);
          }
          return Ok(ConnectionExit::Dropped {
            error: "gateway heartbeat was not acknowledged".to_string(),
            saw_ready,
          });
        }
        acked = false;
        if let Err(error) = sender.send(WsMessage::Text(json!({"op": 1, "d": last_sequence}).to_string())).await {
        return closed_exit(error, saw_ready);
        }
        next_heartbeat = Instant::now() + interval;
      }
      incoming = receive_json(&mut receiver, saw_ready) => {
        let payload = match incoming? {
          Ok(payload) => payload,
          Err(exit) => return Ok(exit),
        };
        if payload.get("s").is_some_and(|sequence| !sequence.is_null()) {
          last_sequence = payload["s"].clone();
        }
        match payload.get("op").and_then(Value::as_u64) {
          Some(1) => {
            if let Err(error) = sender.send(WsMessage::Text(json!({"op": 1, "d": last_sequence}).to_string())).await {
          return closed_exit(error, saw_ready);
            }
          }
          Some(11) => acked = true,
          Some(7 | 9) => {
            let op = payload["op"].as_u64().expect("the reconnect op was matched");
            return Ok(ConnectionExit::Dropped { error: format!("gateway op {op}"), saw_ready });
          }
          Some(0) if payload.get("t").and_then(Value::as_str) == Some("READY") => {
            saw_ready = true;
            bot_user_id = payload
              .get("d")
              .and_then(|data| data.get("user"))
              .and_then(|user| user.get("id"))
              .and_then(Value::as_str)
              .context("Discord READY has no bot user id")?
              .to_string();
            tracing::info!(event = "discord_listener_connected");
            backfill_followed_threads(features, Arc::clone(&client)).await?;
          }
          Some(0) if payload.get("t").and_then(Value::as_str) == Some("MESSAGE_CREATE") => {
            let message = payload.get("d").context("Discord MESSAGE_CREATE has no data")?;
            let result = handle_message_create(message, features, Arc::clone(&client), &bot_user_id).await;
            match result {
              Ok(session) => tracing::info!(
                event = "discord_listener_message_handled",
                channel = ?message.get("channel_id"),
                session = ?session,
              ),
              Err(error) => tracing::error!(
                event = "discord_listener_message_handle_failed",
                channel = ?message.get("channel_id"),
                error = %error,
              ),
            }
          }
          _ => {}
        }
      }
    }
  }
}

async fn receive_json(receiver: &mut Receiver, saw_ready: bool) -> anyhow::Result<Result<Value, ConnectionExit>> {
  loop {
    match receiver.recv().await {
      None => return Ok(Err(ConnectionExit::Dropped { error: "gateway connection closed".to_string(), saw_ready })),
      Some(Err(error)) => return Ok(Err(closed_exit(error, saw_ready)?)),
      Some(Ok(WsMessage::Text(text))) => return Ok(Ok(serde_json::from_str(&text)?)),
      Some(Ok(WsMessage::Binary(bytes))) => return Ok(Ok(serde_json::from_slice(&bytes)?)),
      Some(Ok(WsMessage::Close(_))) => continue,
    }
  }
}

fn closed_exit(error: ws_client::Error, saw_ready: bool) -> anyhow::Result<ConnectionExit> {
  let ws_client::Error::ConnectionClosed { received, sent, .. } = &error else {
    return Err(error.into());
  };
  let code = received.as_ref().or(sent.as_ref()).map(|close| close.code);
  if let Some(code) = code.filter(|code| stop_reason(*code).is_some()) {
    return Ok(ConnectionExit::Stopped { code });
  }
  Ok(ConnectionExit::Dropped { error: error.to_string(), saw_ready })
}

fn is_closed(error: &ws_client::Error) -> bool {
  matches!(error, ws_client::Error::ConnectionClosed { .. })
}

fn stop_reason(code: u16) -> Option<&'static str> {
  STOP_CLOSE_CODES.iter().find_map(|(candidate, reason)| (*candidate == code).then_some(*reason))
}

fn identify_payload(token: &str) -> Value {
  let os = match std::env::consts::OS {
    "macos" => "darwin",
    "windows" => "win32",
    other => other,
  };
  json!({
    "op": 2,
    "d": {
      "token": token,
      "intents": INTENTS,
      "properties": {"os": os, "browser": "charlie-bot", "device": "charlie-bot"},
    },
  })
}

fn heartbeat_delay(interval: Duration) -> anyhow::Result<Duration> {
  let mut bytes = [0_u8; 8];
  getrandom::fill(&mut bytes)?;
  let random = u64::from_ne_bytes(bytes);
  let nanos = ((interval.as_nanos() * u128::from(random)) >> 64) as u64;
  Ok(Duration::from_nanos(nanos))
}

fn next_backoff(backoff: Duration) -> Duration {
  backoff.saturating_mul(2).min(MAX_BACKOFF)
}

/// Re-run the shared thread follow backfill after each gateway READY (`_backfill_followed_threads`).
async fn backfill_followed_threads(
  features: &Arc<FeatureContext>,
  client: Arc<DiscordClient>,
) -> anyhow::Result<usize> {
  let adapter: Arc<dyn ThreadAdapter> = Arc::new(DiscordThreadAdapter::with_client(
    Bot::new(Arc::clone(features), crate::discord_client::BASE_URL),
    client.as_ref().clone(),
  ));
  Ok(thread_entry::backfill_followed_threads(&adapter, features).await?)
}

#[cfg(test)]
#[path = "discord_listener_tests.rs"]
mod tests;
