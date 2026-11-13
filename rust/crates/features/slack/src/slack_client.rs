//! Slack Web API calls used by the Socket Mode listener and the shared thread adapter.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use charliebot_compat::http_client::{self, Settings};
use charliebot_core::text_repr;
use charliebot_registry::FeatureContext;
use http::Method;
use reqwest::{RequestBuilder, Response};
use serde_json::{Value, json};

pub(crate) const API_BASE: &str = "https://slack.com/api/";
// The shared HTTP client's timeout; every Web API call takes it.
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

// The Slack Web API method names. A call site and the user-token allow-list read the name from here, so a renamed
// method reaches the call, the call's error text and the allow-list together.
pub(crate) const AUTH_TEST: &str = "auth.test";
pub(crate) const APPS_CONNECTIONS_OPEN: &str = "apps.connections.open";
pub(crate) const CHAT_POST_MESSAGE: &str = "chat.postMessage";
pub(crate) const CONVERSATIONS_HISTORY: &str = "conversations.history";
pub(crate) const CONVERSATIONS_INFO: &str = "conversations.info";
pub(crate) const CONVERSATIONS_LIST: &str = "conversations.list";
pub(crate) const CONVERSATIONS_OPEN: &str = "conversations.open";
pub(crate) const CONVERSATIONS_REPLIES: &str = "conversations.replies";
pub(crate) const REACTIONS_ADD: &str = "reactions.add";
pub(crate) const REACTIONS_REMOVE: &str = "reactions.remove";
pub(crate) const SEARCH_MESSAGES: &str = "search.messages";
pub(crate) const USERS_LIST: &str = "users.list";

// The Slack Web API field names: the channel a call addresses, the message text, the thread timestamp that makes a
// post a reply, and the users a conversation opens for. The client and the draft sender both build these bodies, so
// both read the names from here.
pub(crate) const CHANNEL: &str = "channel";
pub(crate) const TEXT: &str = "text";
pub(crate) const THREAD_TS: &str = "thread_ts";
pub(crate) const USERS: &str = "users";

/// The Slack Web API wrapper.
pub struct SlackClient {
  http: reqwest::Client,
  api_base: String,
  bot_token: String,
  app_token: String,
  channel_name_cache: Mutex<HashMap<String, Option<String>>>,
}

impl SlackClient {
  /// The process client, built from the current feature credentials.
  pub fn from_features(features: &FeatureContext) -> Result<Self> {
    let credentials = features.credentials.get().context("load Slack credentials")?;
    let app_token = credentials
      .get("slack", "app_token")
      .map(charliebot_core::credentials::CredentialValue::value_str)
      .unwrap_or_default();
    Ok(Self::with_client(
      http_client::client(&Settings::from_env())?,
      credentials.require("slack", "bot_token")?,
      app_token,
      API_BASE.to_string(),
    ))
  }

  pub(crate) fn with_client(http: reqwest::Client, bot_token: String, app_token: String, api_base: String) -> Self {
    Self { http, api_base, bot_token, app_token, channel_name_cache: Mutex::new(HashMap::new()) }
  }

  /// The Socket Mode URL, from a POST of `apps.connections.open`.
  pub(crate) async fn open_connection(&self) -> Result<String> {
    let payload = checked_payload(
      self.request(Method::POST, APPS_CONNECTIONS_OPEN, &self.app_token).send().await?,
      APPS_CONNECTIONS_OPEN,
    )
    .await?;
    payload
      .get("url")
      .and_then(Value::as_str)
      .map(str::to_string)
      .context(format!("{APPS_CONNECTIONS_OPEN} response has no string url"))
  }

  /// Slack's response payload to a POST of one thread reply.
  pub(crate) async fn post_message(&self, channel: &str, text: &str, thread_ts: &str, post_key: &str) -> Result<Value> {
    let body = json!({
      (CHANNEL): channel,
      (TEXT): text,
      (THREAD_TS): thread_ts,
      "metadata": {"event_type": "charliebot_post", "event_payload": {"key": post_key}},
    });
    let response = self.json_request(CHAT_POST_MESSAGE, &self.bot_token, body)?.send().await?;
    checked_payload(response, CHAT_POST_MESSAGE).await
  }

  /// The message timestamp, from a POST of one bot DM.
  pub(crate) async fn post_dm(&self, channel: &str, text: &str) -> Result<String> {
    let body = json!({
      (CHANNEL): channel,
      (TEXT): text,
    });
    let response = self.json_request(CHAT_POST_MESSAGE, &self.bot_token, body)?.send().await?;
    let payload = checked_payload(response, CHAT_POST_MESSAGE).await?;
    let ts = payload
      .get("ts")
      .or_else(|| payload.pointer("/message/ts"))
      .and_then(Value::as_str)
      .unwrap_or_default()
      .to_string();
    Ok(ts)
  }

  /// The authenticated bot identity, which recognizes its metadata-keyed posts.
  pub(crate) async fn auth_test(&self) -> Result<Value> {
    let response = self.request(Method::POST, AUTH_TEST, &self.bot_token).send().await?;
    checked_payload(response, AUTH_TEST).await
  }

  /// The summon's `eyes` reaction, added through `reactions.add`.
  pub(crate) async fn add_reaction(&self, channel: &str, name: &str, ts: &str) -> Result<Value> {
    let body = json!({(CHANNEL): channel, "name": name, "timestamp": ts});
    let response = self.json_request(REACTIONS_ADD, &self.bot_token, body)?.send().await?;
    checked_payload(response, REACTIONS_ADD).await
  }

  /// Remove the summon reaction; `no_reaction` already means the desired state.
  pub(crate) async fn remove_reaction(&self, channel: &str, name: &str, ts: &str) -> Result<Value> {
    let body = json!({(CHANNEL): channel, "name": name, "timestamp": ts});
    let response = self.json_request(REACTIONS_REMOVE, &self.bot_token, body)?.send().await?;
    let payload = response_json(response).await?;
    if !payload.get("ok").is_some_and(text_repr::is_truthy)
      && payload.get("error").and_then(Value::as_str) != Some("no_reaction")
    {
      bail!("{REACTIONS_REMOVE} failed: {}", text_repr::value_str(&payload));
    }
    Ok(payload)
  }

  /// One thread's messages, in Slack order.
  pub(crate) async fn get_thread_replies(&self, channel: &str, thread_ts: &str) -> Result<Vec<Value>> {
    let mut messages = Vec::new();
    let mut cursor = None;
    loop {
      let mut url = reqwest::Url::parse(&format!("{}{CONVERSATIONS_REPLIES}", self.api_base))?;
      {
        let mut query = url.query_pairs_mut();
        query
          .append_pair(CHANNEL, channel)
          .append_pair("ts", thread_ts)
          .append_pair("include_all_metadata", "true")
          .append_pair("limit", "100");
        if let Some(cursor) = cursor.as_deref() {
          query.append_pair("cursor", cursor);
        }
      }
      let response = self.request_url(Method::GET, url.as_str(), &self.bot_token).send().await?;
      let payload = checked_payload(response, CONVERSATIONS_REPLIES).await?;
      messages.extend(
        payload
          .get("messages")
          .and_then(Value::as_array)
          .context(format!("{CONVERSATIONS_REPLIES} response has no messages list"))?
          .iter()
          .cloned(),
      );
      cursor = payload
        .pointer("/response_metadata/next_cursor")
        .and_then(Value::as_str)
        .filter(|cursor| !cursor.is_empty())
        .map(str::to_string);
      if cursor.is_none() {
        return Ok(messages);
      }
    }
  }

  /// The cached channel name or a fresh lookup; a failed lookup is logged in the two classes the listener watches.
  pub(crate) async fn get_channel_name(&self, channel_id: &str) -> Option<String> {
    if let Some(name) =
      self.channel_name_cache.lock().expect("the Slack channel-name cache is never poisoned").get(channel_id)
    {
      return name.clone();
    }

    let resolved = self.resolve_channel_name(channel_id).await;
    let name = match resolved {
      Ok(Some(name)) => Some(name),
      Ok(None) => None,
      Err(error) => {
        tracing::warn!(event = "slack_channel_name_resolve_failed", channel = channel_id, error = %error);
        None
      }
    };
    self
      .channel_name_cache
      .lock()
      .expect("the Slack channel-name cache is never poisoned")
      .insert(channel_id.to_string(), name.clone());
    name
  }

  async fn resolve_channel_name(&self, channel_id: &str) -> Result<Option<String>> {
    let mut url = reqwest::Url::parse(&format!("{}{CONVERSATIONS_INFO}", self.api_base))?;
    url.query_pairs_mut().append_pair(CHANNEL, channel_id);
    let response = self.request_url(Method::GET, url.as_str(), &self.bot_token).send().await?;
    let payload = response_json(response).await?;
    if !payload.get("ok").is_some_and(text_repr::is_truthy) {
      tracing::warn!(
        event = "slack_channel_name_unresolved",
        channel = channel_id,
        error = ?payload.get("error"),
      );
      return Ok(None);
    }
    let name = payload
      .get(CHANNEL)
      .and_then(Value::as_object)
      .and_then(|channel| channel.get("name"))
      .context(format!("{CONVERSATIONS_INFO} response has no channel.name"))?;
    Ok((!name.is_null()).then(|| text_repr::value_str(name)))
  }

  fn request(&self, method: Method, path: &str, token: &str) -> RequestBuilder {
    self.request_url(method, &format!("{}{path}", self.api_base), token)
  }

  fn request_url(&self, method: Method, url: &str, token: &str) -> RequestBuilder {
    self.http.request(method, url).bearer_auth(token).timeout(HTTP_TIMEOUT)
  }

  fn json_request(&self, path: &str, token: &str, body: Value) -> Result<RequestBuilder> {
    Ok(
      self
        .request(Method::POST, path, token)
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(serde_json::to_vec(&body)?),
    )
  }
}

async fn checked_payload(response: Response, method: &str) -> Result<Value> {
  let payload = response_json(response).await?;
  if !payload.get("ok").is_some_and(text_repr::is_truthy) {
    bail!("{method} failed: {}", text_repr::value_str(&payload));
  }
  Ok(payload)
}

async fn response_json(response: Response) -> Result<Value> {
  let response = response.error_for_status()?;
  let bytes = response.bytes().await?;
  serde_json::from_slice(&bytes).context("Slack Web API response is not JSON")
}

#[cfg(test)]
mod tests {
  use crate::test_support::FakeApi;

  #[tokio::test]
  async fn post_dm_sends_chat_post_message_with_channel_and_text() {
    let api = FakeApi::start(&[]).await;
    let ts = api.client.post_dm("U_TEST_USER", "Approval needed").await.expect("DM sends");
    assert!(!ts.is_empty());
    let calls = api.calls();
    let post_call =
      calls.iter().find(|(m, p, _)| m == "POST" && p == "/api/chat.postMessage").expect("postMessage called");
    assert_eq!(post_call.2["channel"], "U_TEST_USER");
    assert_eq!(post_call.2["text"], "Approval needed");
  }
}
