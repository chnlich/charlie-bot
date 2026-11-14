//! The Feishu long-connection listener reads the bot's identity once for a service start, then connects to the
//! endpoint Feishu names and pings at the interval Feishu sets. It acknowledges each event before it handles the
//! event, and hands the parsed event to the shared core through the adapter. A dropped connection reconnects with
//! backoff, and every connection runs the thread follow backfill.
//!
//! The ack comes first because the core may read a whole thread before it answers, and Feishu re-pushes an event that
//! is not acknowledged within three seconds.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use charliebot_chat_threads::channel_service::ChannelListener;
use charliebot_chat_threads::listener_connection::{CLOSE_TIMEOUT, INITIAL_BACKOFF, next_backoff};
use charliebot_chat_threads::thread_entry::ThreadAdapter;
use charliebot_compat::ws_client::{self, Message as WsMessage, Options, Receiver, Sender};
use charliebot_registry::FeatureContext;
use charliebot_triggers::triggers::TriggerManager;
use futures_util::future::BoxFuture;
use serde_json::Value;

use crate::adapter::{self, Bot, FeishuThreadAdapter};
use crate::client::{BASE_URL, BOT_CAPABILITY_CODE, FeishuAPIError, FeishuClient};
use crate::config::FeishuConfig;
use crate::event::{self, Drop, Inbound};
use crate::frame::{Frame, METHOD_DATA, Reassembler, TYPE_EVENT};
use crate::owners;
use crate::package::NAME;

/// The listener implementation passed to the shared `ChannelService` (`run_listener`).
pub(crate) struct FeishuListener;

impl ChannelListener for FeishuListener {
  fn run_listener(&self, features: Arc<FeatureContext>) -> BoxFuture<'static, anyhow::Result<()>> {
    Box::pin(async move { run_listener(features).await })
  }

  fn backfill_lost_summons(&self, features: Arc<FeatureContext>) -> BoxFuture<'static, anyhow::Result<usize>> {
    Box::pin(async move { adapter::backfill_lost_summons(&features).await })
  }
}

/// The state one connection needs: the shared context, the app's client, the bot's open_id, and the service id the
/// endpoint URL names, which every ping carries.
struct Connection {
  features: Arc<FeatureContext>,
  client: FeishuClient,
  bot_open_id: String,
  service_id: i32,
}

/// The long-connection listener, which runs until the process stops it (`run_listener`). The bot's identity is read
/// once for the service start; a missing bot capability stops the listener before it opens the long connection.
async fn run_listener(features: Arc<FeatureContext>) -> anyhow::Result<()> {
  let bot = Bot::new(Arc::clone(&features), BASE_URL);
  let client = bot.client()?;
  let _trigger_manager = features.startup.require::<Arc<TriggerManager>>()?;
  let options = Options {
    open_timeout: ws_client::WEBSOCKETS_16_DEFAULTS.open_timeout,
    close_timeout: Some(CLOSE_TIMEOUT),
    max_size: None,
    ping_interval: ws_client::WEBSOCKETS_16_DEFAULTS.ping_interval,
    ping_timeout: ws_client::WEBSOCKETS_16_DEFAULTS.ping_timeout,
  };
  run_listener_loop(features, client, options, ws_client::Settings::from_env()).await
}

/// The bot identity read once per service start. Feishu answers 11205 while the app has no bot capability, which only
/// the developer console fixes, so the listener logs one error and stops instead of retrying. Any other failure,
/// a network error among them, fails the start, as Discord's preflight does.
async fn preflight(client: &FeishuClient) -> anyhow::Result<Option<String>> {
  match client.bot_open_id().await {
    Ok(open_id) => Ok(Some(open_id)),
    Err(error) if bot_capability_off(&error) => {
      tracing::error!(
        event = "feishu_listener_bot_capability_off",
        cause = "the Feishu app has no bot capability, so the bot identity read failed",
        error = %error,
        fix = "enable the bot capability for the app in the Feishu developer console, publish a version, and restart the server"
      );
      Ok(None)
    }
    Err(error) => Err(error),
  }
}

/// Whether the error is Feishu's answer for an app without the bot capability (`BOT_CAPABILITY_CODE`).
fn bot_capability_off(error: &anyhow::Error) -> bool {
  error.downcast_ref::<FeishuAPIError>().is_some_and(|error| error.code == Some(BOT_CAPABILITY_CODE))
}

/// The reconnect loop the service start reaches once the bot identity is known. A connection that opens or drops
/// retries with backoff from 1 s to 30 s; the identity itself is never read again.
async fn run_listener_loop(
  features: Arc<FeatureContext>,
  client: FeishuClient,
  options: Options,
  settings: ws_client::Settings,
) -> anyhow::Result<()> {
  let Some(bot_open_id) = preflight(&client).await? else {
    return Ok(());
  };
  let mut backoff = INITIAL_BACKOFF;
  loop {
    let endpoint = match client.long_connection_endpoint().await {
      Ok(endpoint) => endpoint,
      Err(error) => {
        tracing::warn!(event = "feishu_listener_connect_failed", error = %error);
        tokio::time::sleep(backoff).await;
        backoff = next_backoff(backoff);
        continue;
      }
    };
    let service_id = service_id_of(&endpoint.url)?;
    let (sender, receiver) = match ws_client::connect(&endpoint.url, &options, &settings).await {
      Ok(connection) => connection,
      Err(error) => {
        tracing::warn!(event = "feishu_listener_connect_failed", error = %error);
        tokio::time::sleep(backoff).await;
        backoff = next_backoff(backoff);
        continue;
      }
    };
    tracing::info!(event = "feishu_listener_connected");
    backoff = INITIAL_BACKOFF;
    let connection = Connection {
      features: Arc::clone(&features),
      client: client.clone(),
      bot_open_id: bot_open_id.clone(),
      service_id,
    };
    let result = run_connection(&sender, receiver, &connection, endpoint.ping_interval).await;
    if let Err(error) = sender.close().await {
      tracing::warn!(event = "feishu_listener_connection_close_failed", error = %error);
    }
    if let Err(error) = result {
      tracing::warn!(event = "feishu_listener_connection_dropped", error = %error);
    }
    tokio::time::sleep(backoff).await;
    backoff = next_backoff(backoff);
  }
}

/// One connection, served until it fails: the follow backfill runs first, then pings and frames are handled as they
/// come.
async fn run_connection(
  sender: &Sender,
  mut receiver: Receiver,
  connection: &Connection,
  ping_interval: Duration,
) -> anyhow::Result<()> {
  adapter::backfill_followed_threads(&connection.features, connection.client.clone()).await?;
  let mut ping = tokio::time::interval(ping_interval);
  let mut reassembler = Reassembler::default();
  loop {
    tokio::select! {
      _ = ping.tick() => {
        sender.send(WsMessage::Binary(Frame::ping(connection.service_id).to_bytes())).await?;
      }
      incoming = receiver.recv() => {
        match incoming {
          None => bail!("the long connection closed"),
          Some(Err(error)) => return Err(error.into()),
          Some(Ok(WsMessage::Binary(bytes))) => handle_frame(&bytes, sender, &mut reassembler, connection).await?,
          Some(Ok(WsMessage::Close(_))) => bail!("the long connection sent a close frame"),
          Some(Ok(WsMessage::Text(_))) => bail!("the long connection sent a text frame, not a protobuf frame"),
        }
      }
    }
  }
}

/// The handling of one binary frame: pongs and other control frames need nothing; an event is acknowledged, then
/// handled.
async fn handle_frame(
  bytes: &[u8],
  sender: &Sender,
  reassembler: &mut Reassembler,
  connection: &Connection,
) -> anyhow::Result<()> {
  let frame = Frame::from_bytes(bytes)?;
  if frame.method != METHOD_DATA || frame.kind() != Some(TYPE_EVENT) {
    return Ok(());
  }
  let Some(payload) = reassembler.accept(&frame, Instant::now())? else {
    return Ok(());
  };
  // The processing time is not known yet: the ack goes out before the event is handled, so it reads 0.
  sender.send(WsMessage::Binary(frame.ack(0).to_bytes())).await?;
  if let Err(error) = handle_event(&payload, connection).await {
    tracing::error!(event = "feishu_listener_message_handle_failed", error = %error);
  }
  Ok(())
}

/// The parse of one event payload, dropping what the rules drop and routing the rest to the shared core. The payload
/// is not logged: it carries the event's verification token.
async fn handle_event(payload: &[u8], connection: &Connection) -> anyhow::Result<()> {
  let event: Value = serde_json::from_slice(payload).context("the event payload is not JSON")?;
  let config = connection.features.config.get()?;
  let allowed = &config.section::<FeishuConfig>(NAME).allowed_users;
  let inbound = admit(event::classify(&event, &connection.bot_open_id)?, &connection.client, allowed).await?;
  if let Inbound::Drop(reason) = inbound {
    tracing::debug!(event = "feishu_listener_event_dropped", reason = ?reason);
    return Ok(());
  }
  let bot = Bot::new(Arc::clone(&connection.features), BASE_URL);
  let adapter: Arc<dyn ThreadAdapter> = Arc::new(FeishuThreadAdapter::with_client(bot, connection.client.clone()));
  let session = adapter::route_inbound(&adapter, &connection.features, inbound).await?;
  tracing::info!(event = "feishu_listener_message_handled", session = ?session);
  Ok(())
}

/// The admission of a classified summon or follow only when its group has a listed person among its members. The
/// member list is read fresh, so a failed read is an error and the event is not handled. Other verdicts pass through
/// unchanged.
async fn admit(inbound: Inbound, client: &FeishuClient, allowed: &BTreeMap<String, String>) -> anyhow::Result<Inbound> {
  let (Inbound::Summon(message) | Inbound::Follow(message)) = &inbound else {
    return Ok(inbound);
  };
  if owners::chat_has_owner(client, allowed, &message.chat_id).await? {
    Ok(inbound)
  } else {
    Ok(Inbound::Drop(Drop::NoOwnerInChat))
  }
}

/// The service id the endpoint URL names in its `service_id` query parameter.
fn service_id_of(url: &str) -> anyhow::Result<i32> {
  let parsed = reqwest::Url::parse(url).context("the endpoint URL does not parse")?;
  let value = parsed
    .query_pairs()
    .find(|(name, _)| name == "service_id")
    .map(|(_, value)| value.into_owned())
    .context("the endpoint URL names no service_id")?;
  value.parse::<i32>().with_context(|| format!("the endpoint service_id is not an integer: {value:?}"))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::event::Message;
  use crate::test_support::{Answer, Home, serve};
  use charliebot_compat::http_client;
  use charliebot_core::test_support::LogCapture;
  use serde_json::json;

  const OWNER: &str = "ou_synthetic_owner";
  const UNLISTED: &str = "ou_synthetic_member";

  fn allowed() -> BTreeMap<String, String> {
    BTreeMap::from([(OWNER.to_string(), "Owner".to_string())])
  }

  /// A message from `sender`, as the classifier hands it on.
  fn message_from(sender: &str, root_id: Option<&str>, thread_id: Option<&str>) -> Message {
    Message {
      tenant_key: "tk_synthetic".to_string(),
      chat_id: "oc_synthetic_admit".to_string(),
      message_id: "om_admit".to_string(),
      root_id: root_id.map(str::to_string),
      thread_id: thread_id.map(str::to_string),
      create_time: "1700000000000".to_string(),
      sender_open_id: sender.to_string(),
      mentions: Vec::new(),
    }
  }

  fn summon_from(sender: &str) -> Inbound {
    Inbound::Summon(message_from(sender, None, None))
  }

  fn follow_from(sender: &str) -> Inbound {
    Inbound::Follow(message_from(sender, Some("om_synthetic_root"), Some("omt_synthetic_thread")))
  }

  fn client(base_url: &str, app_id: &str) -> FeishuClient {
    let http = http_client::client(&http_client::Settings::from_env()).expect("the HTTP client builds");
    FeishuClient::new(http, base_url, app_id, "synthetic-secret")
  }

  fn members(open_ids: &[&str]) -> Value {
    let items: Vec<Value> = open_ids.iter().map(|id| json!({"member_id": id, "member_id_type": "open_id"})).collect();
    json!({"code": 0, "data": {"items": items, "has_more": false}})
  }

  #[tokio::test]
  async fn a_listed_person_in_the_chat_admits_an_unlisted_members_summon() {
    let stub = serve(move |_| Answer::json(members(&[UNLISTED, OWNER]))).await;
    let inbound = admit(summon_from(UNLISTED), &client(&stub.base_url, "cli_admit_present"), &allowed()).await.unwrap();
    assert!(matches!(inbound, Inbound::Summon(_)), "got {inbound:?}");
  }

  #[tokio::test]
  async fn a_listed_person_in_the_chat_admits_an_unlisted_members_follow() {
    let stub = serve(move |_| Answer::json(members(&[UNLISTED, OWNER]))).await;
    let inbound = admit(follow_from(UNLISTED), &client(&stub.base_url, "cli_admit_follow"), &allowed()).await.unwrap();
    assert!(matches!(inbound, Inbound::Follow(_)), "got {inbound:?}");
  }

  #[tokio::test]
  async fn a_chat_with_no_listed_person_drops_the_summon() {
    let stub = serve(move |_| Answer::json(members(&[UNLISTED]))).await;
    let inbound = admit(summon_from(UNLISTED), &client(&stub.base_url, "cli_admit_absent"), &allowed()).await.unwrap();
    assert_eq!(inbound, Inbound::Drop(Drop::NoOwnerInChat));
  }

  #[tokio::test]
  async fn a_member_list_error_is_an_error_not_a_verdict() {
    let stub = serve(|_| Answer::json(json!({"code": 99991672, "msg": "no permission"}))).await;
    let result = admit(summon_from(UNLISTED), &client(&stub.base_url, "cli_admit_error"), &allowed()).await;
    assert!(result.is_err());
  }

  #[test]
  fn the_service_id_comes_from_the_endpoint_query() {
    let url = "wss://open.feishu.cn/ws/synthetic?device_id=d1&service_id=42";
    assert_eq!(service_id_of(url).unwrap(), 42);
    assert!(service_id_of("wss://open.feishu.cn/ws/synthetic?device_id=d1").is_err());
  }

  // The listener takes its own app id: the tenant token cache is keyed by it, so no test reads another test's token.

  #[tokio::test]
  async fn a_no_bot_capability_answer_stops_the_listener_before_the_endpoint_call() {
    let stub = serve(|_| Answer::json(json!({"code": BOT_CAPABILITY_CODE, "msg": "app do not have bot"}))).await;
    let home = Home::new();
    let http = http_client::client(&http_client::Settings::from_env()).expect("the HTTP client builds");
    let client = FeishuClient::new(http, stub.base_url.clone(), "cli_listener_bot", "synthetic-secret");
    let capture = LogCapture::default();
    let _guard = capture.install();
    let options = Options {
      open_timeout: ws_client::WEBSOCKETS_16_DEFAULTS.open_timeout,
      close_timeout: Some(CLOSE_TIMEOUT),
      max_size: None,
      ping_interval: ws_client::WEBSOCKETS_16_DEFAULTS.ping_interval,
      ping_timeout: ws_client::WEBSOCKETS_16_DEFAULTS.ping_timeout,
    };
    tokio::time::timeout(
      Duration::from_secs(2),
      run_listener_loop(Arc::clone(&home.features), client, options, ws_client::Settings::from_env()),
    )
    .await
    .expect("the listener stops instead of retrying")
    .expect("the stop is a plain return, not an error");
    let text = capture.text();
    assert_eq!(text.matches("feishu_listener_bot_capability_off").count(), 1, "one error event: {text}");
    assert!(text.contains("developer console"), "the event names the console fix: {text}");
    let requests = stub.requests.lock().expect("the request log is never poisoned");
    assert!(requests.iter().any(|request| request.target == "/open-apis/bot/v3/info"));
    assert!(
      !requests.iter().any(|request| request.target.starts_with("/callback/ws/endpoint")),
      "the long-connection endpoint is never requested: {:?}",
      requests.iter().map(|request| &request.target).collect::<Vec<_>>()
    );
  }
}
