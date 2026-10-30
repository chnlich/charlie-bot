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

const API_BASE: &str = "https://slack.com/api/";
// `src/infra/http.py::get_http_client` constructs `httpx.AsyncClient()` without an override.
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

/// The Slack Web API wrapper (`slack_listener.SlackClient`).
pub struct SlackClient {
  http: reqwest::Client,
  api_base: String,
  bot_token: String,
  app_token: String,
  channel_name_cache: Mutex<HashMap<String, Option<String>>>,
}

impl SlackClient {
  /// Build the process client from the current feature credentials.
  pub fn from_features(features: &FeatureContext) -> Result<Self> {
    let credentials = features.credentials.get().context("load Slack credentials")?;
    Ok(Self::with_client(
      http_client::client(&Settings::from_env())?,
      credentials.require("slack", "bot_token")?,
      credentials.require("slack", "app_token")?,
      API_BASE.to_string(),
    ))
  }

  pub(crate) fn with_client(http: reqwest::Client, bot_token: String, app_token: String, api_base: String) -> Self {
    Self { http, api_base, bot_token, app_token, channel_name_cache: Mutex::new(HashMap::new()) }
  }

  /// POST `apps.connections.open` and return the Socket Mode URL.
  pub(crate) async fn open_connection(&self) -> Result<String> {
    let payload = checked_payload(
      self.request(Method::POST, "apps.connections.open", &self.app_token).send().await?,
      "apps.connections.open",
    )
    .await?;
    payload
      .get("url")
      .and_then(Value::as_str)
      .map(str::to_string)
      .context("apps.connections.open response has no string url")
  }

  /// POST one thread reply and return Slack's response payload.
  pub(crate) async fn post_message(&self, channel: &str, text: &str, thread_ts: &str, post_key: &str) -> Result<Value> {
    let body = json!({
      "channel": channel,
      "text": text,
      "thread_ts": thread_ts,
      "metadata": {"event_type": "charliebot_post", "event_payload": {"key": post_key}},
    });
    let response = self.json_request("chat.postMessage", &self.bot_token, body)?.send().await?;
    checked_payload(response, "chat.postMessage").await
  }

  /// Read the authenticated bot identity used to recognize its metadata-keyed posts.
  pub(crate) async fn auth_test(&self) -> Result<Value> {
    let response = self.request(Method::POST, "auth.test", &self.bot_token).send().await?;
    checked_payload(response, "auth.test").await
  }

  /// Add the summon's `eyes` reaction.
  pub(crate) async fn add_reaction(&self, channel: &str, name: &str, ts: &str) -> Result<Value> {
    let body = json!({"channel": channel, "name": name, "timestamp": ts});
    let response = self.json_request("reactions.add", &self.bot_token, body)?.send().await?;
    checked_payload(response, "reactions.add").await
  }

  /// Remove the summon reaction; `no_reaction` already means the desired state.
  pub(crate) async fn remove_reaction(&self, channel: &str, name: &str, ts: &str) -> Result<Value> {
    let body = json!({"channel": channel, "name": name, "timestamp": ts});
    let response = self.json_request("reactions.remove", &self.bot_token, body)?.send().await?;
    let payload = response_json(response).await?;
    if !payload.get("ok").is_some_and(text_repr::is_truthy)
      && payload.get("error").and_then(Value::as_str) != Some("no_reaction")
    {
      bail!("reactions.remove failed: {}", text_repr::value_str(&payload));
    }
    Ok(payload)
  }

  /// Read one thread's messages in Slack order.
  pub(crate) async fn get_thread_replies(&self, channel: &str, thread_ts: &str) -> Result<Vec<Value>> {
    let mut messages = Vec::new();
    let mut cursor = None;
    loop {
      let mut url = reqwest::Url::parse(&format!("{}conversations.replies", self.api_base))?;
      {
        let mut query = url.query_pairs_mut();
        query
          .append_pair("channel", channel)
          .append_pair("ts", thread_ts)
          .append_pair("include_all_metadata", "true")
          .append_pair("limit", "100");
        if let Some(cursor) = cursor.as_deref() {
          query.append_pair("cursor", cursor);
        }
      }
      let response = self.request_url(Method::GET, url.as_str(), &self.bot_token).send().await?;
      let payload = checked_payload(response, "conversations.replies").await?;
      messages.extend(
        payload
          .get("messages")
          .and_then(Value::as_array)
          .context("conversations.replies response has no messages list")?
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

  /// Resolve and cache a channel name, logging the same two failure classes as the Python client.
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
    let mut url = reqwest::Url::parse(&format!("{}conversations.info", self.api_base))?;
    url.query_pairs_mut().append_pair("channel", channel_id);
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
      .get("channel")
      .and_then(Value::as_object)
      .and_then(|channel| channel.get("name"))
      .context("conversations.info response has no channel.name")?;
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
