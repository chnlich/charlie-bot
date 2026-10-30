//! The Discord REST client (`src/features/discord/discord_client.py`): one method per v10 endpoint, the bot token in an
//! `Authorization: Bot` header, the 429 policy in one place, and `DiscordAPIError` for a failed call. The helpers that
//! reason about ids, links, the intent and the permission bits are pure functions.

use std::fmt::{self, Display};
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Context, bail};
use charliebot_core::text_repr;
use http::{Method, StatusCode, header};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};

/// The pinned major version of Discord's REST API every path hits.
pub const BASE_URL: &str = "https://discord.com/api/v10";

/// The application flag that unlocks message text over the gateway, and its limited variant.
const MESSAGE_CONTENT_FLAG: u64 = 1 << 18;
const MESSAGE_CONTENT_LIMITED_FLAG: u64 = 1 << 19;

/// The permission that grants everything else; a bot holding it never reports missing permissions.
pub const ADMINISTRATOR: u64 = 1 << 3;

/// The permission bits the Discord entrypoint needs, in table order, per
/// https://discord.com/developers/docs/topics/permissions.
pub const REQUIRED_PERMISSIONS: &[(&str, u64)] = &[
  ("VIEW_CHANNEL", 1 << 10),
  ("SEND_MESSAGES", 1 << 11),
  ("SEND_MESSAGES_IN_THREADS", 1 << 38),
  ("CREATE_PUBLIC_THREADS", 1 << 35),
  ("READ_MESSAGE_HISTORY", 1 << 16),
  ("ADD_REACTIONS", 1 << 6),
];

/// Discord's cap on a thread name; a longer name is cut, not refused.
const THREAD_NAME_MAX_CHARS: usize = 100;

/// The archive duration `start_thread_from_message` sends (`auto_archive_duration`'s default): seven days, in minutes.
const AUTO_ARCHIVE_DURATION_MINUTES: u64 = 10080;

/// Retries of one call after a 429; a third rate-limit answer gives up.
const MAX_429_RETRIES: usize = 2;

/// The page size of a message read, Discord's cap on one call.
const MESSAGE_PAGE_LIMIT: usize = 100;

/// The `User-Agent` every call sends.
const USER_AGENT_VALUE: &str = "DiscordBot (https://discord.com, charlie-bot)";

/// The timeout of each REST call. Python's `DiscordClient` passes none, so every call takes the shared httpx client's
/// default: `httpx.AsyncClient()` in `src/infra/http.py:18-21`, which is `Timeout(5.0)` in httpx 0.28.1 (checked in the
/// worktree `.venv`). httpx bounds each phase of a call; reqwest's timeout bounds the whole call.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Characters `urllib.parse.quote(emoji, safe='')` leaves unencoded: ASCII letters, digits and `_.-~`.
const QUOTE_SET: &AsciiSet = &NON_ALPHANUMERIC.remove(b'_').remove(b'.').remove(b'-').remove(b'~');

/// `^https://discord\.com/channels/(\d+)/(\d+)(?:/(\d+))?$`, with Python's `$` also matching before a final newline.
static MESSAGE_LINK: LazyLock<Regex> = LazyLock::new(|| {
  Regex::new(r"^https://discord\.com/channels/([0-9]+)/([0-9]+)(?:/([0-9]+))?\n?$").expect("the link pattern is valid")
});

/// A Discord id as an integer. Ids are compared and sorted as numbers, never as strings, whose order flips whenever the
/// digit counts differ. A non-numeric id panics, as Python's `int()` raises.
pub fn snowflake_key(snowflake: &str) -> u64 {
  snowflake.parse().unwrap_or_else(|error| panic!("a Discord id is an unsigned integer, got {snowflake:?}: {error}"))
}

/// The ids of one discord.com channel or message link; `message_id` is None for a channel link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageLink {
  pub guild_id: String,
  pub channel_id: String,
  pub message_id: Option<String>,
}

/// Parse one discord.com channel or message link. Any other text fails with the `ValueError` message Python prints.
pub fn parse_message_link(url: &str) -> anyhow::Result<MessageLink> {
  let Some(captures) = MESSAGE_LINK.captures(url) else {
    bail!("not a discord.com channel/message link: {}", text_repr::str_repr(url));
  };
  Ok(MessageLink {
    guild_id: captures[1].to_string(),
    channel_id: captures[2].to_string(),
    message_id: captures.get(3).map(|id| id.as_str().to_string()),
  })
}

/// True when the application flags grant the message-content intent, the full flag or the limited one.
pub fn message_content_intent_enabled(application_flags: u64) -> bool {
  application_flags & (MESSAGE_CONTENT_FLAG | MESSAGE_CONTENT_LIMITED_FLAG) != 0
}

/// The names in `REQUIRED_PERMISSIONS` that the permission bitfield lacks, in table order; empty under ADMINISTRATOR.
pub fn missing_permissions(permissions: u64) -> Vec<&'static str> {
  if permissions & ADMINISTRATOR != 0 {
    return Vec::new();
  }
  REQUIRED_PERMISSIONS.iter().filter(|(_, bit)| permissions & bit == 0).map(|(name, _)| *name).collect()
}

/// One failed Discord REST call: its method, path, HTTP status, Discord's error code (None when the body has none) and
/// the human message. The bot token never appears in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscordAPIError {
  pub method: String,
  pub path: String,
  pub status: u16,
  pub code: Option<i64>,
  pub message: String,
}

impl Display for DiscordAPIError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let code = self.code.map_or_else(|| "None".to_string(), |code| code.to_string());
    write!(
      f,
      "{} {} failed: HTTP {} code={code} message={}",
      self.method,
      self.path,
      self.status,
      text_repr::str_repr(&self.message)
    )
  }
}

impl std::error::Error for DiscordAPIError {}

/// The wait a 429 asks for: the body's `retry_after` seconds, else the `Retry-After` header, else zero
/// (`_retry_after_seconds`). A header that is not a number fails the call.
pub fn retry_after_seconds(body: &[u8], retry_after_header: Option<&str>) -> anyhow::Result<f64> {
  if let Ok(Value::Object(object)) = serde_json::from_slice::<Value>(body)
    && let Some(seconds) = object.get("retry_after").and_then(Value::as_f64)
  {
    return Ok(seconds);
  }
  match retry_after_header {
    None => Ok(0.0),
    Some(text) => text.trim().parse::<f64>().with_context(|| format!("Retry-After is not a number: {text:?}")),
  }
}

/// A Discord user, as the messages, the bot's own user and the guild readbacks name it.
#[derive(Debug, Clone, Deserialize)]
pub struct User {
  pub id: String,
  pub username: String,
  #[serde(default)]
  pub global_name: Option<String>,
  #[serde(default)]
  pub bot: bool,
}

/// One file attached to a message.
#[derive(Debug, Clone, Deserialize)]
pub struct Attachment {
  pub url: String,
}

/// One message object of the REST API.
#[derive(Debug, Clone, Deserialize)]
pub struct Message {
  pub id: String,
  pub author: User,
  pub timestamp: String,
  #[serde(default)]
  pub content: Option<String>,
  #[serde(default)]
  pub attachments: Vec<Attachment>,
  #[serde(rename = "type", default)]
  pub kind: Option<i64>,
  #[serde(default)]
  pub webhook_id: Option<String>,
}

/// A channel or thread object of the REST API.
#[derive(Debug, Clone, Deserialize)]
pub struct Channel {
  pub id: String,
  #[serde(rename = "type")]
  pub kind: i64,
  #[serde(default)]
  pub name: Option<String>,
  #[serde(default)]
  pub parent_id: Option<String>,
}

/// The bot's application, whose flags carry the message-content intent.
#[derive(Debug, Clone, Deserialize)]
pub struct Application {
  pub id: String,
  pub flags: u64,
}

/// A guild the bot is in; `permissions` is the bot's bitfield as Discord writes it, a decimal string.
#[derive(Debug, Clone, Deserialize)]
pub struct Guild {
  pub id: String,
  pub name: String,
  pub permissions: String,
}

/// The Discord v10 REST wrapper: one method per API call, all of them through `request` for the headers, the error
/// mapping and the 429 retry.
#[derive(Clone)]
pub struct DiscordClient {
  http: reqwest::Client,
  base_url: String,
  token: String,
}

impl DiscordClient {
  /// A client that calls `base_url` (`BASE_URL` in production) with the bot token.
  pub fn new(http: reqwest::Client, base_url: String, bot_token: String) -> Self {
    Self { http, base_url, token: bot_token }
  }

  /// One Discord REST call: sent with the bot headers, retried at most `MAX_429_RETRIES` times after a 429 wait, and
  /// answered with the decoded JSON body (None when a 2xx answer has none). Any other non-2xx answer fails with a
  /// `DiscordAPIError`.
  async fn request(
    &self,
    method: Method,
    path: &str,
    json_body: Option<Value>,
    params: &[(&str, String)],
  ) -> anyhow::Result<Option<Value>> {
    let mut url = reqwest::Url::parse(&format!("{}{path}", self.base_url))?;
    if !params.is_empty() {
      url.query_pairs_mut().extend_pairs(params.iter().map(|(name, value)| (*name, value.as_str())));
    }
    for retries in 0..=MAX_429_RETRIES {
      let mut request = self
        .http
        .request(method.clone(), url.clone())
        .timeout(REQUEST_TIMEOUT)
        .header(header::AUTHORIZATION, format!("Bot {}", self.token))
        .header(header::USER_AGENT, USER_AGENT_VALUE);
      if let Some(body) = &json_body {
        request = request.header(header::CONTENT_TYPE, "application/json").body(serde_json::to_vec(body)?);
      }
      let response = request.send().await?;
      let status = response.status();
      if status == StatusCode::TOO_MANY_REQUESTS && retries < MAX_429_RETRIES {
        let header_text =
          response.headers().get(header::RETRY_AFTER).and_then(|value| value.to_str().ok()).map(str::to_string);
        let body = response.bytes().await?;
        let wait = retry_after_seconds(&body, header_text.as_deref())?;
        let wait = Duration::try_from_secs_f64(wait.max(0.0)).context("retry_after is out of range")?;
        tokio::time::sleep(wait).await;
        continue;
      }
      let body = response.bytes().await?;
      if status.is_success() {
        return if body.is_empty() { Ok(None) } else { Ok(Some(serde_json::from_slice(&body)?)) };
      }
      let parsed: Option<Value> = serde_json::from_slice(&body).ok();
      let object = parsed.as_ref().and_then(Value::as_object);
      let code = object.and_then(|object| object.get("code")).and_then(Value::as_i64);
      let message = match object.and_then(|object| object.get("message")).and_then(Value::as_str) {
        Some(message) => message.to_string(),
        None => String::from_utf8_lossy(&body).into_owned(),
      };
      return Err(
        DiscordAPIError { method: method.to_string(), path: path.to_string(), status: status.as_u16(), code, message }
          .into(),
      );
    }
    unreachable!("the last attempt returns")
  }

  /// A call whose 2xx answer must carry a body.
  async fn request_body(
    &self,
    method: Method,
    path: &str,
    json_body: Option<Value>,
    params: &[(&str, String)],
  ) -> anyhow::Result<Value> {
    self.request(method, path, json_body, params).await?.with_context(|| format!("{path} answered with no body"))
  }

  /// GET /gateway/bot: the `wss:` gateway URL to connect to.
  pub async fn get_gateway_url(&self) -> anyhow::Result<String> {
    let payload = self.request_body(Method::GET, "/gateway/bot", None, &[]).await?;
    payload["url"].as_str().map(str::to_string).context("the gateway answer names no url")
  }

  /// GET /applications/@me: the bot's application, its flags included.
  pub async fn get_current_application(&self) -> anyhow::Result<Application> {
    let body = self.request_body(Method::GET, "/applications/@me", None, &[]).await?;
    Ok(serde_json::from_value(body)?)
  }

  /// GET /users/@me: the bot's own user.
  pub async fn get_current_user(&self) -> anyhow::Result<User> {
    let body = self.request_body(Method::GET, "/users/@me", None, &[]).await?;
    Ok(serde_json::from_value(body)?)
  }

  /// GET /users/@me/guilds: the guilds the bot is in.
  pub async fn list_current_user_guilds(&self) -> anyhow::Result<Vec<Guild>> {
    let body = self.request_body(Method::GET, "/users/@me/guilds", None, &[]).await?;
    Ok(serde_json::from_value(body)?)
  }

  /// GET /channels/{channel_id}: the channel object.
  pub async fn get_channel(&self, channel_id: &str) -> anyhow::Result<Channel> {
    let body = self.request_body(Method::GET, &format!("/channels/{channel_id}"), None, &[]).await?;
    Ok(serde_json::from_value(body)?)
  }

  /// POST start-thread-from-message: a thread rooted at one message, its name cut to Discord's cap.
  pub async fn start_thread_from_message(
    &self,
    channel_id: &str,
    message_id: &str,
    name: &str,
  ) -> anyhow::Result<Channel> {
    let name: String = name.chars().take(THREAD_NAME_MAX_CHARS).collect();
    let body = json!({"name": name, "auto_archive_duration": AUTO_ARCHIVE_DURATION_MINUTES});
    let path = format!("/channels/{channel_id}/messages/{message_id}/threads");
    let answer = self.request_body(Method::POST, &path, Some(body), &[]).await?;
    Ok(serde_json::from_value(answer)?)
  }

  /// POST /channels/{channel_id}/messages: a text-only message. `allowed_mentions` is always `{"parse": []}`, so a
  /// reply pings nobody. Returns the created message object.
  pub async fn create_message(&self, channel_id: &str, content: &str) -> anyhow::Result<Value> {
    let body = json!({"content": content, "allowed_mentions": {"parse": []}});
    self.request_body(Method::POST, &format!("/channels/{channel_id}/messages"), Some(body), &[]).await
  }

  /// Create one message with a stable post key for Discord's nonce deduplication.
  pub async fn create_message_with_post_key(
    &self,
    channel_id: &str,
    content: &str,
    post_key: &str,
  ) -> anyhow::Result<Value> {
    let nonce = &charliebot_store::control_events::sha256_hex(post_key)[..25];
    let body = json!({
      "content": content,
      "allowed_mentions": {"parse": []},
      "nonce": nonce,
      "enforce_nonce": true,
    });
    self.request_body(Method::POST, &format!("/channels/{channel_id}/messages"), Some(body), &[]).await
  }

  /// PUT the bot's own reaction, the emoji URL-encoded, onto one message.
  pub async fn add_reaction(&self, channel_id: &str, message_id: &str, emoji: &str) -> anyhow::Result<()> {
    let path = reaction_path(channel_id, message_id, emoji);
    self.request(Method::PUT, &path, None, &[]).await?;
    Ok(())
  }

  /// DELETE the bot's own reaction, the emoji URL-encoded, from one message.
  pub async fn remove_own_reaction(&self, channel_id: &str, message_id: &str, emoji: &str) -> anyhow::Result<()> {
    let path = reaction_path(channel_id, message_id, emoji);
    self.request(Method::DELETE, &path, None, &[]).await?;
    Ok(())
  }

  /// GET /channels/{channel_id}/messages/{message_id}: one message.
  pub async fn get_message(&self, channel_id: &str, message_id: &str) -> anyhow::Result<Message> {
    let body =
      self.request_body(Method::GET, &format!("/channels/{channel_id}/messages/{message_id}"), None, &[]).await?;
    Ok(serde_json::from_value(body)?)
  }

  /// GET /channels/{channel_id}/messages with `after` and `limit`, sorted oldest first by integer id. Discord answers
  /// newest first.
  pub async fn get_messages(
    &self,
    channel_id: &str,
    after: Option<&str>,
    limit: usize,
  ) -> anyhow::Result<Vec<Message>> {
    let mut params = vec![("limit", limit.to_string())];
    if let Some(after) = after {
      params.push(("after", after.to_string()));
    }
    let body = self.request_body(Method::GET, &format!("/channels/{channel_id}/messages"), None, &params).await?;
    let mut messages: Vec<Message> = serde_json::from_value(body)?;
    messages.sort_by_key(|message| snowflake_key(&message.id));
    Ok(messages)
  }

  /// Every message of one channel, oldest first, read 100 at a time from id "0": a page shorter than the limit ends
  /// the read.
  pub async fn all_messages(&self, channel_id: &str) -> anyhow::Result<Vec<Message>> {
    let mut messages = Vec::new();
    let mut after = "0".to_string();
    loop {
      let page = self.get_messages(channel_id, Some(&after), MESSAGE_PAGE_LIMIT).await?;
      let short = page.len() < MESSAGE_PAGE_LIMIT;
      if let Some(last) = page.last() {
        after = last.id.clone();
      }
      messages.extend(page);
      if short {
        return Ok(messages);
      }
    }
  }
}

/// The reaction path of one message, the emoji percent-encoded as `urllib.parse.quote(emoji, safe='')` writes it.
fn reaction_path(channel_id: &str, message_id: &str, emoji: &str) -> String {
  let emoji = utf8_percent_encode(emoji, QUOTE_SET);
  format!("/channels/{channel_id}/messages/{message_id}/reactions/{emoji}/@me")
}

#[cfg(test)]
mod tests;
