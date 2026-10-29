//! Slack Socket Mode events and the Slack face of the shared thread core.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use charliebot_chat_threads::channel_service::ChannelListener;
use charliebot_chat_threads::thread_entry::{
  self, Summon, ThreadAdapter, ThreadMessage, ThreadPlatform, ThreadReplyError,
};
use charliebot_compat::ws_client::{self, Message, Options, Receiver, Sender, WEBSOCKETS_16_DEFAULTS};
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::pyfmt::python_truthy;
use charliebot_registry::FeatureContext;
use charliebot_triggers::triggers::TriggerManager;
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::config::SlackConfig;
pub use crate::slack_client::SlackClient;
use crate::thread_contract::SLACK;

const SLACK_NS: Uuid =
  Uuid::from_bytes([0x1b, 0x4e, 0x28, 0xba, 0x2f, 0xa1, 0x4d, 0x7a, 0x9f, 0x0c, 0x8d, 0x5e, 0x7a, 0x3b, 0x6c, 0x11]);
const ACCEPTANCE_REACTION: &str = "eyes";
const MAX_BACKOFF: Duration = Duration::from_secs(30);
// `src/infra/timeouts.py::WS_CLIENT_CLOSE_TIMEOUT` is one second; the sender is aborted on every session exit.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(1);

/// Return the deterministic session id for a Slack thread (`summon_session_id`).
pub fn summon_session_id(team_id: &str, channel_id: &str, thread_ts: &str) -> String {
  Uuid::new_v5(&SLACK_NS, format!("slack:{team_id}:{channel_id}:{thread_ts}").as_bytes()).to_string()
}

/// Accept one allowed `app_mention`, persist the summon, and start its round.
pub async fn handle_app_mention(
  event: &Value,
  features: &Arc<FeatureContext>,
  client: Arc<SlackClient>,
) -> Result<Option<String>, ThreadReplyError> {
  let config = features.config.get().map_err(ThreadReplyError::failure)?;
  let slack_config = config.section::<SlackConfig>("slack");
  let channel_id = event.get("channel");
  let thread_ts = truthy_text(event.get("thread_ts")).or_else(|| truthy_text(event.get("ts")));
  let user = event.get("user").and_then(Value::as_str);

  if event.get("type").and_then(Value::as_str) != Some("app_mention")
    || !user.is_some_and(|user| slack_config.allowed_user_ids.iter().any(|allowed| allowed == user))
  {
    tracing::debug!(
      event = "slack_mention_dropped",
      channel = ?channel_id,
      thread_ts = ?thread_ts,
      slack_user = ?user,
    );
    return Ok(None);
  }

  let channel_id = required_text(event, "channel")?;
  let thread_ts = thread_ts.ok_or_else(|| ThreadReplyError::failure("Slack app_mention has no ts"))?;
  let mention_ts = required_text(event, "ts")?;
  let team_id = truthy_text(event.get("team"))
    .or_else(|| truthy_text(event.get("team_id")))
    .ok_or_else(|| ThreadReplyError::failure("Slack app_mention has no team id"))?;
  let session_id = summon_session_id(team_id, channel_id, thread_ts);
  let label = format!("Slack #{}", client.get_channel_name(channel_id).await.as_deref().unwrap_or(channel_id));
  let origin = json!({"team_id": team_id, "channel_id": channel_id, "thread_ts": thread_ts});
  let block = json!({"channel_id": channel_id, "thread_ts": thread_ts, "mention_ts": mention_ts});
  let adapter: Arc<dyn ThreadAdapter> = Arc::new(SlackThreadAdapter::with_client(Arc::clone(features), client));
  let result = thread_entry::accept_summon(
    &adapter,
    features,
    Summon {
      session_id: &session_id,
      label: &label,
      origin: &origin,
      block: &block,
      content: SLACK.summon_line,
      user: Some(user.expect("the allowed mention has a user")),
    },
  )
  .await?;
  Ok(Some(result))
}

/// Whether one raw Socket Mode message is an allowed, human-authored Slack message.
fn eligible_thread_message(message: &Value, allowed_user_ids: &[String]) -> bool {
  message.get("subtype").is_none_or(Value::is_null)
    && message.get("bot_id").is_none_or(Value::is_null)
    && message
      .get("user")
      .and_then(Value::as_str)
      .is_some_and(|user| allowed_user_ids.iter().any(|allowed| allowed == user))
}

/// Accept one thread message and arm its followed session when it is eligible.
pub async fn handle_thread_message(
  event: &Value,
  features: &Arc<FeatureContext>,
  client: Arc<SlackClient>,
) -> Result<Option<String>, ThreadReplyError> {
  if event.get("subtype").is_some_and(|subtype| !subtype.is_null()) {
    return Ok(None);
  }
  let Some(thread_ts) = truthy_text(event.get("thread_ts")) else {
    return Ok(None);
  };
  let config = features.config.get().map_err(ThreadReplyError::failure)?;
  let slack_config = config.section::<SlackConfig>("slack");
  if event.get("bot_id").is_some_and(|bot_id| !bot_id.is_null())
    || !eligible_thread_message(event, &slack_config.allowed_user_ids)
  {
    return Ok(None);
  }
  let channel_id = required_text(event, "channel")?;
  let message_ts = required_text(event, "ts")?;
  let team_id = truthy_text(event.get("team"))
    .or_else(|| truthy_text(event.get("team_id")))
    .ok_or_else(|| ThreadReplyError::failure("Slack message has no team id"))?;
  let session_id = summon_session_id(team_id, channel_id, thread_ts);
  let adapter: Arc<dyn ThreadAdapter> = Arc::new(SlackThreadAdapter::with_client(Arc::clone(features), client));
  let channel_id = channel_id.to_string();
  thread_entry::follow_message(&adapter, features, &session_id, message_ts, move |origin| {
    origin.get("channel_id").and_then(Value::as_str) == Some(&channel_id)
  })
  .await
}

/// Backfill unread messages for every followed Slack thread on a socket connection.
pub async fn backfill_followed_threads(
  features: &Arc<FeatureContext>,
  client: Arc<SlackClient>,
) -> Result<usize, ThreadReplyError> {
  let adapter: Arc<dyn ThreadAdapter> = Arc::new(SlackThreadAdapter::with_client(Arc::clone(features), client));
  thread_entry::backfill_followed_threads(&adapter, features).await
}

/// Refuse a reply while eligible Slack messages remain above the session watermark.
pub async fn assert_thread_fresh(session_id: &str, features: &Arc<FeatureContext>) -> Result<(), ThreadReplyError> {
  let adapter: Arc<dyn ThreadAdapter> = Arc::new(SlackThreadAdapter::lazy(Arc::clone(features)));
  thread_entry::assert_thread_fresh(&adapter, features, session_id).await
}

/// Post the session's reply to its Slack thread.
pub async fn post_reply(
  session_id: &str,
  text: &str,
  features: &Arc<FeatureContext>,
) -> Result<Value, ThreadReplyError> {
  let adapter: Arc<dyn ThreadAdapter> = Arc::new(SlackThreadAdapter::lazy(Arc::clone(features)));
  thread_entry::post_reply(&adapter, features, session_id, text).await
}

/// Ack eligible messages and advance the session's Slack watermark.
pub async fn ack_messages(
  session_id: &str,
  message_ids: &[String],
  features: &Arc<FeatureContext>,
) -> Result<Value, ThreadReplyError> {
  let adapter: Arc<dyn ThreadAdapter> = Arc::new(SlackThreadAdapter::lazy(Arc::clone(features)));
  thread_entry::ack_messages(&adapter, features, session_id, message_ids).await
}

/// Run the round-end audit after one `master_done` event.
pub async fn deliver_done(
  session_id: &str,
  done: &Value,
  features: &Arc<FeatureContext>,
) -> Result<bool, ThreadReplyError> {
  let adapter: Arc<dyn ThreadAdapter> = Arc::new(SlackThreadAdapter::lazy(Arc::clone(features)));
  thread_entry::deliver_done(&adapter, features, session_id, done).await
}

/// Report queued Slack summons lost across a restart.
pub async fn backfill_lost_summons(features: &Arc<FeatureContext>) -> Result<usize, ThreadReplyError> {
  let adapter: Arc<dyn ThreadAdapter> = Arc::new(SlackThreadAdapter::lazy(Arc::clone(features)));
  thread_entry::backfill_lost_summons(&adapter, features).await
}

/// The Socket Mode listener registered with `ChannelService`.
pub(crate) struct SlackListener;

impl ChannelListener for SlackListener {
  fn run_listener(&self, features: Arc<FeatureContext>) -> BoxFuture<'static, Result<()>> {
    Box::pin(async move { run_listener(features).await })
  }

  fn backfill_lost_summons(&self, features: Arc<FeatureContext>) -> BoxFuture<'static, Result<usize>> {
    Box::pin(async move { backfill_lost_summons(&features).await.map_err(anyhow::Error::from) })
  }
}

/// Run Socket Mode until the service cancels this task.
pub async fn run_listener(features: Arc<FeatureContext>) -> Result<()> {
  let client = Arc::new(SlackClient::from_features(&features)?);
  let _trigger_manager = features.startup.require::<Arc<TriggerManager>>()?;
  run_listener_loop(features, client, ws_client::Settings::from_env()).await
}

async fn run_listener_loop(
  features: Arc<FeatureContext>,
  client: Arc<SlackClient>,
  ws_settings: ws_client::Settings,
) -> Result<()> {
  let mut backoff = Duration::from_secs(1);
  let options = Options { max_size: None, close_timeout: Some(CLOSE_TIMEOUT), ..WEBSOCKETS_16_DEFAULTS };
  loop {
    let url = match client.open_connection().await {
      Ok(url) => url,
      Err(error) => {
        tracing::warn!(event = "slack_listener_connect_failed", error = %error);
        tokio::time::sleep(backoff).await;
        backoff = next_backoff(backoff);
        continue;
      }
    };
    backoff = Duration::from_secs(1);

    match ws_client::connect(&url, &options, &ws_settings).await {
      Ok((sender, receiver)) => {
        let result = serve_socket_mode(&sender, receiver, &features, Arc::clone(&client)).await;
        sender.abort();
        if let Err(error) = result {
          tracing::warn!(event = "slack_listener_connection_dropped", error = %error);
        }
      }
      Err(error) => {
        tracing::warn!(event = "slack_listener_connection_dropped", error = %error);
      }
    }

    tokio::time::sleep(backoff).await;
    backoff = next_backoff(backoff);
  }
}

async fn serve_socket_mode(
  sender: &Sender,
  mut receiver: Receiver,
  features: &Arc<FeatureContext>,
  client: Arc<SlackClient>,
) -> Result<()> {
  expect_hello(&mut receiver).await?;
  tracing::info!(event = "slack_listener_connected");
  backfill_followed_threads(features, Arc::clone(&client)).await?;

  while let Some(message) = receiver.recv().await {
    let envelope = decode_envelope(message?)?;
    let envelope_id = envelope.get("envelope_id");
    if envelope.get("type").and_then(Value::as_str) == Some("disconnect") {
      tracing::info!(event = "slack_listener_disconnect");
      return Ok(());
    }
    tracing::debug!(event = "slack_listener_envelope", envelope_id = ?envelope_id);
    if let Some(envelope_id) = envelope_id.filter(|id| !id.is_null()) {
      sender.send(Message::Text(json!({"envelope_id": envelope_id}).to_string())).await?;
    }

    let inner = (envelope.get("type").and_then(Value::as_str) == Some("events_api"))
      .then(|| {
        envelope.get("payload").filter(|payload| python_truthy(payload)).and_then(|payload| payload.get("event"))
      })
      .flatten();
    let Some(inner) = inner.filter(|event| python_truthy(event)) else {
      continue;
    };
    match inner.get("type").and_then(Value::as_str) {
      Some("app_mention") => {
        let channel = inner.get("channel");
        let thread_ts = truthy_text(inner.get("thread_ts")).or_else(|| truthy_text(inner.get("ts")));
        let slack_user = inner.get("user");
        match handle_app_mention(inner, features, Arc::clone(&client)).await {
          Ok(session) => tracing::info!(
            event = "slack_listener_app_mention_handled",
            channel = ?channel,
            thread_ts = ?thread_ts,
            slack_user = ?slack_user,
            session = ?session,
          ),
          Err(error) => tracing::error!(
            event = "slack_listener_app_mention_handle_failed",
            channel = ?channel,
            thread_ts = ?thread_ts,
            slack_user = ?slack_user,
            error = %error,
          ),
        }
      }
      Some("message") => {
        if let Err(error) = handle_thread_message(inner, features, Arc::clone(&client)).await {
          tracing::error!(
            event = "slack_listener_message_handle_failed",
            channel = ?inner.get("channel"),
            thread_ts = ?inner.get("thread_ts"),
            slack_user = ?inner.get("user"),
            error = %error,
          );
        }
      }
      _ => {}
    }
  }
  Ok(())
}

async fn expect_hello(receiver: &mut Receiver) -> Result<()> {
  let message = receiver.recv().await.context("Socket Mode connection ended before hello")??;
  let envelope = decode_envelope(message)?;
  let received = envelope.get("type").and_then(Value::as_str);
  if received != Some("hello") {
    tracing::warn!(event = "slack_listener_expected_hello", received = ?received);
  }
  Ok(())
}

fn decode_envelope(message: Message) -> Result<Value> {
  match message {
    Message::Text(text) => serde_json::from_str(&text).context("Socket Mode frame is not JSON"),
    Message::Binary(bytes) => serde_json::from_slice(&bytes).context("Socket Mode frame is not JSON"),
    Message::Close(frame) => bail!("Socket Mode closed before the next event: {frame:?}"),
  }
}

fn next_backoff(backoff: Duration) -> Duration {
  backoff.saturating_mul(2).min(MAX_BACKOFF)
}

fn required_text<'a>(event: &'a Value, field: &str) -> Result<&'a str, ThreadReplyError> {
  event
    .get(field)
    .and_then(Value::as_str)
    .ok_or_else(|| ThreadReplyError::failure(format!("Slack event has no string {field}")))
}

fn truthy_text(value: Option<&Value>) -> Option<&str> {
  value.and_then(Value::as_str).filter(|text| !text.is_empty())
}

/// The Slack implementation of the shared thread adapter.
pub(crate) struct SlackThreadAdapter {
  features: Arc<FeatureContext>,
  client: Mutex<Option<Arc<SlackClient>>>,
  identity: Mutex<Option<Value>>,
}

impl SlackThreadAdapter {
  pub(crate) fn lazy(features: Arc<FeatureContext>) -> Self {
    Self { features, client: Mutex::new(None), identity: Mutex::new(None) }
  }

  pub(crate) fn with_client(features: Arc<FeatureContext>, client: Arc<SlackClient>) -> Self {
    Self { features, client: Mutex::new(Some(client)), identity: Mutex::new(None) }
  }

  fn client(&self) -> Result<Arc<SlackClient>> {
    let mut client = self.client.lock().expect("the Slack adapter client slot is never poisoned");
    if client.is_none() {
      *client = Some(Arc::new(SlackClient::from_features(&self.features)?));
    }
    Ok(Arc::clone(client.as_ref().expect("the Slack adapter client is initialized")))
  }
}

impl ThreadAdapter for SlackThreadAdapter {
  fn platform(&self) -> &ThreadPlatform {
    &SLACK
  }

  fn post<'a>(&'a self, address: &'a Value, text: &'a str, post_key: &'a str) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move {
      self
        .client()?
        .post_message(object_text(address, "channel_id"), text, object_text(address, "thread_ts"), post_key)
        .await?;
      Ok(())
    })
  }

  fn find_own_post_by_key<'a>(
    &'a self,
    address: &'a Value,
    post_key: &'a str,
    _text: &'a str,
    _created_after: &'a str,
    messages: Option<&'a [ThreadMessage]>,
  ) -> BoxFuture<'a, Result<bool>> {
    Box::pin(async move {
      let client = self.client()?;
      let cached_identity = self.identity.lock().expect("the Slack identity slot is never poisoned").clone();
      let identity = match cached_identity {
        Some(identity) => identity,
        None => {
          let identity = client.auth_test().await?;
          *self.identity.lock().expect("the Slack identity slot is never poisoned") = Some(identity.clone());
          identity
        }
      };
      let thread = match messages {
        Some(messages) => messages.to_vec(),
        None => client
          .get_thread_replies(object_text(address, "channel_id"), object_text(address, "thread_ts"))
          .await?
          .into_iter()
          .map(|message| -> Result<ThreadMessage> {
            Ok(ThreadMessage {
              id: message.get("ts").and_then(Value::as_str).context("Slack thread message has no ts")?.to_string(),
              user: message.get("user").and_then(Value::as_str).map(str::to_string),
              text: message.get("text").and_then(Value::as_str).unwrap_or_default().to_string(),
              created_at: None,
              metadata: message.get("metadata").cloned(),
              bot_id: message.get("bot_id").and_then(Value::as_str).map(str::to_string),
              is_bot: message.get("bot_id").is_some(),
              eligible: false,
            })
          })
          .collect::<Result<Vec<_>>>()?,
      };
      Ok(thread.iter().any(|message| {
        let is_own = identity
          .get("user_id")
          .and_then(Value::as_str)
          .is_some_and(|user_id| message.user.as_deref() == Some(user_id))
          || identity
            .get("bot_id")
            .and_then(Value::as_str)
            .is_some_and(|bot_id| message.bot_id.as_deref() == Some(bot_id));
        let metadata = message.metadata.as_ref();
        let payload = metadata.and_then(|metadata| metadata.get("event_payload"));
        is_own
          && metadata.and_then(|metadata| metadata.get("event_type")).and_then(Value::as_str) == Some("charliebot_post")
          && payload.and_then(|payload| payload.get("key")).and_then(Value::as_str) == Some(post_key)
      }))
    })
  }

  fn add_ack<'a>(&'a self, block: &'a Value) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move {
      self
        .client()?
        .add_reaction(object_text(block, "channel_id"), ACCEPTANCE_REACTION, object_text(block, SLACK.mention_key))
        .await?;
      Ok(())
    })
  }

  fn remove_ack<'a>(&'a self, block: &'a Value) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move {
      self
        .client()?
        .remove_reaction(object_text(block, "channel_id"), ACCEPTANCE_REACTION, object_text(block, SLACK.mention_key))
        .await?;
      Ok(())
    })
  }

  fn read_thread<'a>(
    &'a self,
    origin: &'a Value,
    config: &'a CharlieBotConfig,
  ) -> BoxFuture<'a, Result<Vec<ThreadMessage>>> {
    Box::pin(async move {
      let messages =
        self.client()?.get_thread_replies(object_text(origin, "channel_id"), object_text(origin, "thread_ts")).await?;
      let allowed_user_ids = &config.section::<SlackConfig>("slack").allowed_user_ids;
      messages
        .iter()
        .map(|message| -> Result<ThreadMessage> {
          let id = message.get("ts").and_then(Value::as_str).context("Slack thread message has no ts")?.to_string();
          let user = message.get("user").and_then(Value::as_str).map(str::to_string);
          let text = message
            .get("text")
            .filter(|value| python_truthy(value))
            .map(charliebot_core::pyfmt::python_str)
            .unwrap_or_default();
          Ok(ThreadMessage {
            id,
            user,
            text,
            created_at: None,
            metadata: message.get("metadata").cloned(),
            bot_id: message.get("bot_id").and_then(Value::as_str).map(str::to_string),
            is_bot: message.get("bot_id").is_some(),
            eligible: eligible_thread_message(message, allowed_user_ids),
          })
        })
        .collect()
    })
  }

  fn address_of(&self, origin: &Value) -> Value {
    json!({"channel_id": object_text(origin, "channel_id"), "thread_ts": object_text(origin, "thread_ts")})
  }

  fn log_fields(&self, address: &Value) -> Value {
    let mut fields = Map::new();
    fields.insert("channel".to_string(), Value::String(object_text(address, "channel_id").to_string()));
    fields.insert("thread_ts".to_string(), Value::String(object_text(address, "thread_ts").to_string()));
    Value::Object(fields)
  }
}

fn object_text<'a>(object: &'a Value, field: &str) -> &'a str {
  object.get(field).and_then(Value::as_str).unwrap_or_else(|| panic!("Slack thread data has no string {field}"))
}

#[cfg(test)]
mod tests {
  use std::time::Instant;

  use charliebot_chat_threads::thread_entry::{ThreadAdapter, ThreadDetail};
  use charliebot_store::metadata::SessionStatus;
  use http::StatusCode;
  use serde_json::json;

  use super::*;
  use crate::test_support::{FakeApi, Home, SocketPlan};

  const TS: &str = "1700000000.000100";

  fn mention() -> Value {
    json!({"type": "app_mention", "user": "U_ALLOWED", "team": "T_TEST", "channel": "C_TEST", "ts": TS})
  }

  async fn wait_until(mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
      while !predicate() {
        tokio::time::sleep(Duration::from_millis(10)).await;
      }
    })
    .await
    .expect("the asynchronous side effect arrives");
  }

  #[tokio::test]
  async fn keyed_slack_post_sends_metadata_for_lookup() {
    let api = FakeApi::start(&[]).await;

    api.client.post_message("C_TEST", "hello", TS, "reply#1").await.unwrap();

    assert_eq!(
      api.posts()[0]["metadata"],
      json!({"event_type": "charliebot_post", "event_payload": {"key": "reply#1"}})
    );
  }

  #[tokio::test]
  async fn slack_thread_reads_request_all_metadata_and_one_hundred_messages() {
    let home = Home::new();
    let api = FakeApi::start(&[]).await;
    let adapter = SlackThreadAdapter::with_client(Arc::clone(&home.features), Arc::clone(&api.client));
    let origin = json!({"channel_id": "C_TEST", "thread_ts": TS});
    let config = home.features.config.get().unwrap();

    adapter.read_eligible(&origin, &config).await.unwrap();

    let replies = api.calls().into_iter().find(|call| call.1 == "/api/conversations.replies").unwrap().2;
    assert_eq!(replies["include_all_metadata"], "true");
    assert_eq!(replies["limit"], "100");
  }

  #[tokio::test]
  async fn allowed_user_creates_session_and_persists_agent_message() {
    let home = Home::new();
    let api = FakeApi::start(&[]).await;
    let event = mention();
    let expected_id = summon_session_id("T_TEST", "C_TEST", TS);

    let accepted = handle_app_mention(&event, &home.features, Arc::clone(&api.client)).await.unwrap();
    assert_eq!(accepted.as_deref(), Some(expected_id.as_str()));
    wait_until(|| home.launches().len() == 1 && api.calls().iter().any(|call| call.1 == "/api/reactions.add")).await;

    let metadata = home.features.store.get_session(&expected_id).await.unwrap().unwrap();
    assert!(metadata.name.starts_with("Slack #synthetic-room "), "{}", metadata.name);
    let origin = crate::metadata::slack_origin(&metadata).unwrap().unwrap();
    assert_eq!(
      (origin.team_id.as_str(), origin.channel_id.as_str(), origin.thread_ts.as_str()),
      ("T_TEST", "C_TEST", TS)
    );
    assert_eq!(metadata.group.as_deref(), Some("Slack #synthetic-room"));

    let events = home
      .features
      .store
      .load_chat_events_sync(&expected_id)
      .unwrap()
      .iter()
      .map(charliebot_store::ChatEvent::to_value)
      .collect::<Vec<_>>();
    let summon = events.iter().find(|event| event.get("slack").is_some()).expect("the Slack mention is persisted");
    assert_eq!(summon["content"], SLACK.summon_line);
    assert_eq!(summon["slack"], json!({"channel_id": "C_TEST", "thread_ts": TS, "mention_ts": TS}));
    assert_eq!(home.launches()[0].0, expected_id);
    assert_eq!(home.launches()[0].1[0]["id"], summon["id"]);
    assert!(api.posts().is_empty());
    let reactions: Vec<Value> =
      api.calls().into_iter().filter(|call| call.1 == "/api/reactions.add").map(|call| call.2).collect();
    assert_eq!(reactions, [json!({"channel": "C_TEST", "name": "eyes", "timestamp": TS})]);
    assert_eq!(
      api.calls().iter().filter(|call| call.1 == "/api/conversations.info").count(),
      1,
      "the name is resolved once",
    );
    assert_eq!(
      api.calls().iter().filter(|call| call.1 == "/api/conversations.replies").count(),
      0,
      "the summon does not read thread content",
    );
  }

  #[tokio::test]
  async fn duplicate_mention_reuses_one_summon_and_ack() {
    let home = Home::new();
    let api = FakeApi::start(&[]).await;
    let event = mention();
    let first = handle_app_mention(&event, &home.features, Arc::clone(&api.client)).await.unwrap().unwrap();
    wait_until(|| home.launches().len() == 1).await;
    let second = handle_app_mention(&event, &home.features, Arc::clone(&api.client)).await.unwrap().unwrap();

    assert_eq!(first, second);
    assert_eq!(home.features.store.list_sessions_readonly(SessionStatus::Active).unwrap().len(), 1,);
    assert_eq!(
      home
        .features
        .store
        .load_chat_events_sync(&first)
        .unwrap()
        .iter()
        .filter(
          |event| event.get("type").and_then(charliebot_store::compact::CompactValue::as_str) == Some("agent_message")
        )
        .count(),
      1,
    );
    assert_eq!(home.launches().len(), 1);
    assert_eq!(api.calls().iter().filter(|call| call.1 == "/api/reactions.add").count(), 1);
    assert_eq!(api.calls().iter().filter(|call| call.1 == "/api/conversations.info").count(), 1);
  }

  #[tokio::test]
  async fn accepted_slack_post_timeout_is_recovered_by_its_metadata_key() {
    let home = Home::new();
    let api = FakeApi::start(&[]).await;
    let session = home.thread_session("slack-timeout", TS).await;
    api.set_accepted_post_failure_once();
    let adapter: Arc<dyn ThreadAdapter> =
      Arc::new(SlackThreadAdapter::with_client(Arc::clone(&home.features), Arc::clone(&api.client)));

    let reply = charliebot_chat_threads::thread_entry::post_reply_with_request_id(
      &adapter,
      &home.features,
      &session.id,
      "accepted before the response failed",
      Some("timeout-request"),
    )
    .await
    .unwrap();

    assert_eq!(reply["posted"], true);
    assert_eq!(api.posts().len(), 1);
    assert_eq!(
      api.posts()[0]["metadata"],
      json!({"event_type": "charliebot_post", "event_payload": {"key": "timeout-request#1"}})
    );
    let replies = home.features.store.load_chat_events_sync(&session.id).unwrap();
    assert_eq!(replies.iter().filter(|event| event["type"] == SLACK.reply_event_type).count(), 1);
  }

  #[tokio::test]
  async fn retry_after_reply_event_write_failure_finds_the_accepted_post() {
    let home = Home::new();
    let api = FakeApi::start(&[]).await;
    let session = home.thread_session("slack-event-write", TS).await;
    let adapter: Arc<dyn ThreadAdapter> =
      Arc::new(SlackThreadAdapter::with_client(Arc::clone(&home.features), Arc::clone(&api.client)));
    let fault = charliebot_store::write_faults::inject_chat_append_fault(
      &home.features.store,
      SLACK.reply_event_type,
      &[&session.id],
      1,
      5,
    );

    let first = charliebot_chat_threads::thread_entry::post_reply_with_request_id(
      &adapter,
      &home.features,
      &session.id,
      "post before event write",
      Some("write-request"),
    )
    .await;
    assert!(first.is_err());
    assert_eq!(fault.hits(), [charliebot_store::write_faults::Hit::Failed]);
    drop(fault);

    let second = charliebot_chat_threads::thread_entry::post_reply_with_request_id(
      &adapter,
      &home.features,
      &session.id,
      "post before event write",
      Some("write-request"),
    )
    .await
    .unwrap();

    assert_eq!(second["posted"], true);
    assert_eq!(api.posts().len(), 1);
    let replies = home.features.store.load_chat_events_sync(&session.id).unwrap();
    assert_eq!(replies.iter().filter(|event| event["type"] == SLACK.reply_event_type).count(), 1);
  }

  #[tokio::test]
  async fn retry_of_a_partial_slack_reply_posts_only_the_missing_segment() {
    let home = Home::new();
    let api = FakeApi::start(&[]).await;
    let session = home.thread_session("slack-partial", TS).await;
    let adapter: Arc<dyn ThreadAdapter> =
      Arc::new(SlackThreadAdapter::with_client(Arc::clone(&home.features), Arc::clone(&api.client)));
    let segment = "x".repeat(SLACK.max_post_chars);
    let text = format!("{segment}y");
    api.set_thread_messages(vec![json!({
      "ts": "1700000000.000001",
      "user": "U_BOT",
      "bot_id": "B_BOT",
      "text": segment,
      "metadata": {"event_type": "charliebot_post", "event_payload": {"key": "partial-request#1"}},
    })]);

    let reply = charliebot_chat_threads::thread_entry::post_reply_with_request_id(
      &adapter,
      &home.features,
      &session.id,
      &text,
      Some("partial-request"),
    )
    .await
    .unwrap();

    assert_eq!(reply["chunks"], 2);
    assert_eq!(api.posts().len(), 1);
    assert_eq!(api.posts()[0]["text"], "y");
  }

  #[tokio::test]
  async fn unhandled_event_drops_with_no_side_effects() {
    for event in [
      json!({"type": "app_mention", "user": "U_OTHER", "team": "T_TEST", "channel": "C_TEST", "ts": TS}),
      json!({"type": "message", "user": "U_ALLOWED", "team": "T_TEST", "channel": "C_TEST", "ts": TS}),
    ] {
      let home = Home::new();
      let api = FakeApi::start(&[]).await;
      assert_eq!(handle_app_mention(&event, &home.features, Arc::clone(&api.client)).await.unwrap(), None);
      assert!(api.calls().is_empty());
      assert!(home.features.store.get_session(&summon_session_id("T_TEST", "C_TEST", TS)).await.unwrap().is_none());
    }
  }

  #[tokio::test]
  async fn trigger_master_forwards_input_id_to_the_task_wake() {
    let home = Home::new();
    let api = FakeApi::start(&[]).await;
    let session_id = handle_app_mention(&mention(), &home.features, Arc::clone(&api.client)).await.unwrap().unwrap();
    wait_until(|| home.launches().len() == 1).await;

    let summon = home
      .features
      .store
      .load_chat_events_sync(&session_id)
      .unwrap()
      .iter()
      .find(|event| event.get("slack").is_some())
      .map(|event| event.to_value())
      .expect("the summon is persisted before the task wake");
    assert_eq!(home.launches()[0].1[0]["id"], summon["id"]);
  }

  #[tokio::test]
  async fn remove_reaction_treats_no_reaction_as_success() {
    let api = FakeApi::start(&[]).await;
    api.set_no_reaction();

    let response = api.client.remove_reaction("C_TEST", ACCEPTANCE_REACTION, TS).await.unwrap();

    assert_eq!(response, json!({"ok": false, "error": "no_reaction"}));
  }

  #[tokio::test]
  async fn eligible_thread_message_arms_the_follow_trigger() {
    for watermark in [None, Some(TS)] {
      let home = Home::new();
      let api = FakeApi::start(&[]).await;
      let session_id = summon_session_id("T_TEST", "C_TEST", TS);
      home.thread_session(&session_id, TS).await;
      if let Some(watermark) = watermark {
        home
          .features
          .store
          .update_slot_fields(
            &session_id,
            "slack",
            Map::from_iter([("slack_watermark_ts".to_string(), json!(watermark))]),
          )
          .await
          .unwrap()
          .expect("the Slack session remains present");
      }
      let event = json!({
        "type": "message",
        "team": "T_TEST",
        "channel": "C_TEST",
        "user": "U_ALLOWED",
        "thread_ts": TS,
        "ts": "1700000000.000200",
      });

      let armed = handle_thread_message(&event, &home.features, Arc::clone(&api.client)).await.unwrap();

      assert_eq!(armed.as_deref(), Some(session_id.as_str()));
      let triggers = home.features.startup.require::<Arc<TriggerManager>>().unwrap();
      let records = triggers.list_triggers(&session_id).await.unwrap();
      assert_eq!(records.len(), 1);
      assert_eq!(records[0].message, "slack-thread-follow floor=1700000000.000200");
    }
  }

  #[tokio::test]
  async fn read_eligible_over_the_real_client_keeps_only_allowed_human_messages() {
    let home = Home::new();
    let api = FakeApi::start(&[]).await;
    api.set_thread_messages(vec![
      json!({"type": "message", "user": "U_ALLOWED", "ts": "1700000000.000200", "text": "hi"}),
      json!({"type": "message", "subtype": "bot_message", "bot_id": "B_TEST", "ts": "1700000000.000300", "text": "bot"}),
      json!({"type": "message", "user": "U_OTHER", "ts": "1700000000.000400", "text": "lurker"}),
    ]);
    let adapter = SlackThreadAdapter::with_client(Arc::clone(&home.features), Arc::clone(&api.client));
    let config = home.features.config.get().unwrap();
    let origin = json!({"team_id": "T_TEST", "channel_id": "C_TEST", "thread_ts": TS});
    let messages = adapter.read_eligible(&origin, &config).await.unwrap();

    assert_eq!(
      messages,
      [ThreadMessage {
        id: "1700000000.000200".to_string(),
        user: Some("U_ALLOWED".to_string()),
        text: "hi".to_string(),
        created_at: None,
        metadata: None,
        bot_id: None,
        is_bot: false,
        eligible: true,
      }]
    );
    let call = api.calls().into_iter().find(|call| call.1 == "/api/conversations.replies").unwrap();
    assert_eq!(call.0, "GET");
    assert_eq!(call.2, json!({"channel": "C_TEST", "ts": TS, "include_all_metadata": "true", "limit": "100"}));
  }

  #[tokio::test]
  async fn watermark_persists_through_metadata_json() {
    let home = Home::new();
    let session = home.thread_session("synthetic-watermark-session", TS).await;
    home
      .features
      .store
      .update_slot_fields(
        &session.id,
        "slack",
        Map::from_iter([("slack_watermark_ts".to_string(), json!("1700000000.000050"))]),
      )
      .await
      .unwrap()
      .expect("the Slack session remains present");

    let reloaded = home.features.store.get_session(&session.id).await.unwrap().unwrap();
    assert_eq!(reloaded.extra.get("slack_watermark_ts"), Some(&json!("1700000000.000050")));
  }

  #[tokio::test]
  async fn thread_message_guard_chain_drops() {
    let home = Home::new();
    let api = FakeApi::start(&[]).await;
    let session_id = summon_session_id("T_TEST", "C_TEST", TS);
    home.thread_session(&session_id, TS).await;
    home
      .features
      .store
      .update_slot_fields(&session_id, "slack", Map::from_iter([("slack_watermark_ts".to_string(), json!(TS))]))
      .await
      .unwrap()
      .expect("the Slack session remains present");
    for event in [
      json!({"type": "message", "subtype": "message_changed", "user": "U_ALLOWED", "team": "T_TEST", "channel": "C_TEST", "thread_ts": TS, "ts": "1700000000.000200"}),
      json!({"type": "message", "subtype": "message_deleted", "user": "U_ALLOWED", "team": "T_TEST", "channel": "C_TEST", "thread_ts": TS, "ts": "1700000000.000200"}),
      json!({"type": "message", "bot_id": "B_TEST", "team": "T_TEST", "channel": "C_TEST", "thread_ts": TS, "ts": "1700000000.000200"}),
      json!({"type": "message", "user": "U_OTHER", "team": "T_TEST", "channel": "C_TEST", "thread_ts": TS, "ts": "1700000000.000200"}),
      json!({"type": "message", "user": "U_ALLOWED", "team": "T_TEST", "channel": "C_TEST", "thread_ts": TS, "ts": TS}),
      json!({"type": "message", "user": "U_ALLOWED", "team": "T_TEST", "channel": "C_TEST", "thread_ts": TS, "ts": "1700000000.000040"}),
    ] {
      assert_eq!(handle_thread_message(&event, &home.features, Arc::clone(&api.client)).await.unwrap(), None);
    }
    let triggers = home.features.startup.require::<Arc<TriggerManager>>().unwrap();
    assert!(triggers.list_triggers(&session_id).await.unwrap().is_empty());
    assert!(api.calls().is_empty());
  }

  #[tokio::test]
  async fn reply_gate_refuses_the_stale_thread_and_persists_nothing() {
    let home = Home::new();
    let api = FakeApi::start(&[]).await;
    let session = home.thread_session("synthetic-stale-session", TS).await;
    home
      .features
      .store
      .update_slot_fields(
        &session.id,
        "slack",
        Map::from_iter([("slack_watermark_ts".to_string(), json!("1700000000.000050"))]),
      )
      .await
      .unwrap()
      .expect("the Slack session remains present");
    api.set_thread_messages(vec![
      json!({"user": "U_ALLOWED", "ts": "1700000000.000050", "text": "the summon itself"}),
      json!({"user": "U_ALLOWED", "ts": "1700000000.000110", "text": "new follow up"}),
    ]);
    let events_before = home.features.store.load_chat_events_sync(&session.id).unwrap();

    let adapter: Arc<dyn ThreadAdapter> =
      Arc::new(SlackThreadAdapter::with_client(Arc::clone(&home.features), Arc::clone(&api.client)));
    let error = thread_entry::assert_thread_fresh(&adapter, &home.features, &session.id).await.unwrap_err();

    assert_eq!(error.status, StatusCode::PRECONDITION_FAILED);
    let ThreadDetail::Structured(detail) = error.detail else { panic!("stale thread has a structured refusal") };
    assert_eq!(detail["error"], "stale_thread");
    assert_eq!(detail["watermark_ts"], "1700000000.000050");
    assert_eq!(detail["new_messages"][0]["ts"], "1700000000.000110");
    assert!(api.posts().is_empty());
    assert_eq!(home.features.store.load_chat_events_sync(&session.id).unwrap(), events_before);
  }

  #[tokio::test]
  async fn listener_cancel_skips_the_close_wait() {
    let home = Home::new();
    let api = FakeApi::start(&[SocketPlan::Hold]).await;
    let task = tokio::spawn(run_listener_loop(
      Arc::clone(&home.features),
      Arc::clone(&api.client),
      ws_client::Settings::default(),
    ));
    api.wait_for_sockets(1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let started = Instant::now();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(started.elapsed() < CLOSE_TIMEOUT, "cancel took {:?}", started.elapsed());
  }

  #[tokio::test]
  async fn listener_reconnects_within_three_seconds_after_disconnect() {
    let home = Home::new();
    let api = FakeApi::start(&[SocketPlan::Disconnect, SocketPlan::Hold]).await;
    let task = tokio::spawn(run_listener_loop(
      Arc::clone(&home.features),
      Arc::clone(&api.client),
      ws_client::Settings::default(),
    ));
    api.wait_for_sockets(2).await;
    let times = api.socket_times();

    assert!(times[1].duration_since(times[0]) < Duration::from_secs(3));
    assert_eq!(api.open_calls(), 2);
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
  }

  #[tokio::test]
  async fn listener_reconnects_after_a_dropped_connection() {
    let home = Home::new();
    let api = FakeApi::start(&[SocketPlan::Drop, SocketPlan::Hold]).await;
    let task = tokio::spawn(run_listener_loop(
      Arc::clone(&home.features),
      Arc::clone(&api.client),
      ws_client::Settings::default(),
    ));
    api.wait_for_sockets(2).await;
    assert_eq!(api.open_calls(), 2);
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
  }

  #[tokio::test]
  async fn listener_retries_a_failed_apps_connections_open() {
    let home = Home::new();
    let api = FakeApi::start(&[SocketPlan::Hold]).await;
    api.set_open_failures(1);
    let started = Instant::now();
    let task = tokio::spawn(run_listener_loop(
      Arc::clone(&home.features),
      Arc::clone(&api.client),
      ws_client::Settings::default(),
    ));
    api.wait_for_sockets(1).await;

    assert_eq!(api.open_calls(), 2);
    assert!(started.elapsed() < Duration::from_secs(3));
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
  }

  #[test]
  fn reconnect_backoff_doubles_and_caps_at_thirty_seconds() {
    let mut backoff = Duration::from_secs(1);
    let mut seconds = vec![backoff.as_secs()];
    for _ in 0..7 {
      backoff = next_backoff(backoff);
      seconds.push(backoff.as_secs());
    }
    assert_eq!(seconds, [1, 2, 4, 8, 16, 30, 30, 30]);
  }

  #[test]
  fn the_slack_platform_name_is_a_thread_platform_key() {
    // Python: tests/test_run_token.py:243.
    assert!(charliebot_core::event_types::THREAD_PLATFORM_KEYS.contains(&SLACK.name));
  }
}
