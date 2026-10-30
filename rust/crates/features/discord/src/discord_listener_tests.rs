//! Loopback gateway peers and listener behavior cases for the Discord feature.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{CloseFrame as AxumCloseFrame, Message as AxumMessage, WebSocket, WebSocketUpgrade};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use charliebot_compat::http_client::{self, Settings};
use charliebot_compat::ws_client::{self, Options};
use charliebot_core::event_types;
use charliebot_registry::turn::TurnContribution;
use charliebot_store::metadata::SessionStatus;
use charliebot_store::trigger_files::TriggerStatus;
use charliebot_triggers::triggers::TriggerManager;
use futures_util::SinkExt;
use serde_json::{Value, json};
use tokio::net::TcpListener;

use super::*;
use crate::discord_client::{DiscordClient, message_content_intent_enabled};
use crate::test_support::{ALLOWED_USER, Home, OTHER_USER, TEST_TOKEN};

const GUILD: &str = "900000000000000010";
const PARENT: &str = "900000000000000020";
const THREAD: &str = "900000000000000030";
const MENTION: &str = "900000000000000040";
const BOT_USER: &str = "600000000000000001";
const DM_CHANNEL: &str = "900000000000000050";

#[derive(Clone)]
struct GatewayScript {
  interval_ms: u64,
  after_identify: Vec<Value>,
  close_after_identify: Option<u16>,
  acknowledge_heartbeats: bool,
  close_after_heartbeats: Option<(usize, u16)>,
  /// Close with this code at the first heartbeat that carries a sequence number, which the client sends only
  /// after it has read a sequenced dispatch.
  close_after_sequenced_heartbeat: Option<u16>,
}

impl GatewayScript {
  fn new(after_identify: Vec<Value>) -> Self {
    Self {
      interval_ms: 60_000,
      after_identify,
      close_after_identify: None,
      acknowledge_heartbeats: false,
      close_after_heartbeats: None,
      close_after_sequenced_heartbeat: None,
    }
  }
}

#[derive(Default)]
struct GatewayState {
  scripts: Mutex<VecDeque<GatewayScript>>,
  connections: AtomicUsize,
  received: Mutex<Vec<Vec<Value>>>,
}

struct Gateway {
  url: String,
  state: Arc<GatewayState>,
  task: tokio::task::JoinHandle<()>,
}

impl Gateway {
  async fn start(scripts: Vec<GatewayScript>) -> Self {
    let state = Arc::new(GatewayState::default());
    state.scripts.lock().unwrap().extend(scripts);
    let app = Router::new()
      .route("/gateway", get(upgrade_gateway))
      .route("/gateway/", get(upgrade_gateway))
      .with_state(Arc::clone(&state));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Self { url: format!("ws://{address}/gateway"), state, task }
  }

  fn received(&self) -> Vec<Vec<Value>> {
    self.state.received.lock().unwrap().clone()
  }
}

impl Drop for Gateway {
  fn drop(&mut self) {
    self.task.abort();
  }
}

async fn upgrade_gateway(State(state): State<Arc<GatewayState>>, upgrade: WebSocketUpgrade) -> Response {
  let script = state.scripts.lock().unwrap().pop_front().expect("a gateway script is queued");
  upgrade.on_upgrade(move |socket| serve_gateway(socket, state, script)).into_response()
}

async fn serve_gateway(mut socket: WebSocket, state: Arc<GatewayState>, script: GatewayScript) {
  let connection = state.connections.fetch_add(1, Ordering::SeqCst);
  state.received.lock().unwrap().push(Vec::new());
  socket
    .send(AxumMessage::Text(json!({"op": 10, "d": {"heartbeat_interval": script.interval_ms}}).to_string().into()))
    .await
    .unwrap();
  while let Some(Ok(message)) = socket.recv().await {
    match message {
      AxumMessage::Text(text) => {
        let value: Value = serde_json::from_str(&text).unwrap();
        state.received.lock().unwrap()[connection].push(value.clone());
        match value.get("op").and_then(Value::as_u64) {
          Some(2) => {
            for payload in &script.after_identify {
              socket.send(AxumMessage::Text(payload.to_string().into())).await.unwrap();
            }
            if let Some(code) = script.close_after_identify {
              close_gateway(&mut socket, code).await;
              return;
            }
          }
          Some(1) => {
            let heartbeats = state.received.lock().unwrap()[connection]
              .iter()
              .filter(|payload| payload.get("op").and_then(Value::as_u64) == Some(1))
              .count();
            if script.acknowledge_heartbeats {
              socket.send(AxumMessage::Text(json!({"op": 11, "d": null}).to_string().into())).await.unwrap();
            }
            if let Some((target, code)) = script.close_after_heartbeats
              && heartbeats >= target
            {
              close_gateway(&mut socket, code).await;
              return;
            }
            if let Some(code) = script.close_after_sequenced_heartbeat
              && !value["d"].is_null()
            {
              close_gateway(&mut socket, code).await;
              return;
            }
          }
          _ => {}
        }
      }
      AxumMessage::Close(frame) => {
        state.received.lock().unwrap()[connection].push(json!({
          "close_code": frame.as_ref().map(|close| close.code),
        }));
        socket.flush().await.unwrap();
        return;
      }
      AxumMessage::Ping(payload) => socket.send(AxumMessage::Pong(payload)).await.unwrap(),
      AxumMessage::Pong(_) | AxumMessage::Binary(_) => {}
    }
  }
}

async fn close_gateway(socket: &mut WebSocket, code: u16) {
  socket
    .send(AxumMessage::Close(Some(AxumCloseFrame { code, reason: "synthetic gateway close".into() })))
    .await
    .unwrap();
  tokio::time::timeout(Duration::from_secs(1), async {
    while let Some(Ok(message)) = socket.recv().await {
      if matches!(message, AxumMessage::Close(_)) {
        socket.flush().await.unwrap();
        return;
      }
    }
  })
  .await
  .expect("the client answers the close frame");
}

async fn wait_for(mut condition: impl FnMut() -> bool) {
  tokio::time::timeout(Duration::from_secs(2), async {
    while !condition() {
      tokio::time::sleep(Duration::from_millis(5)).await;
    }
  })
  .await
  .expect("the expected Discord side effect arrives");
}

fn settings() -> Settings {
  Settings { https_proxy: None, no_proxy: Some("127.0.0.1,localhost".to_string()), cert_file: None }
}

fn http_client() -> reqwest::Client {
  http_client::client(&settings()).unwrap()
}

fn discord_client(home: &Home) -> Arc<DiscordClient> {
  Arc::new(DiscordClient::new(http_client(), home.api.clone(), TEST_TOKEN.to_string()))
}

fn options() -> Options {
  Options { max_size: None, close_timeout: Some(Duration::from_secs(1)), ..ws_client::WEBSOCKETS_16_DEFAULTS }
}

fn ready(sequence: u64) -> Value {
  json!({"op": 0, "s": sequence, "t": "READY", "d": {"user": {"id": BOT_USER}}})
}

fn gateway_dispatch(kind: &str, sequence: u64, data: Value) -> Value {
  json!({"op": 0, "s": sequence, "t": kind, "d": data})
}

fn message(id: &str, channel: &str) -> Value {
  json!({
    "id": id,
    "guild_id": GUILD,
    "channel_id": channel,
    "author": {"id": ALLOWED_USER, "bot": false},
    "type": 0,
    "content": format!("<@{BOT_USER}> plan the release"),
    "mentions": [{"id": BOT_USER}],
  })
}

fn origin(thread_id: &str) -> Value {
  json!({"guild_id": GUILD, "parent_channel_id": PARENT, "thread_id": thread_id})
}

fn thread_message(id: &str, user: &str, bot: bool, content: &str) -> Value {
  json!({
    "id": id,
    "channel_id": THREAD,
    "author": {"id": user, "username": "synthetic-user", "bot": bot},
    "content": content,
    "type": 0,
    "timestamp": "2026-08-26T00:00:01Z",
  })
}

fn configure_gateway(home: &Home, gateway: &Gateway, flags: u64) {
  let mut fake = home.fake();
  fake.application = json!({"id": BOT_USER, "flags": flags});
  fake.guilds = json!([]);
  fake.gateway_url = Some(gateway.url.clone());
}

async fn run_one_connection(home: &Home, gateway: &Gateway) -> anyhow::Result<ConnectionExit> {
  let (sender, receiver) = ws_client::connect(&gateway.url, &options(), &settings()).await?;
  let result = run_connection(&sender, receiver, TEST_TOKEN, &home.features, discord_client(home)).await;
  let close_result = sender.close().await;
  if let Err(error) = close_result
    && !is_closed(&error)
  {
    return Err(error.into());
  }
  result
}

#[tokio::test]
async fn test_identify_carries_token_and_intents() {
  let home = Home::new(true).await;
  let gateway =
    Gateway::start(vec![GatewayScript { close_after_identify: Some(4004), ..GatewayScript::new(Vec::new()) }]).await;
  configure_gateway(&home, &gateway, 1 << 18);
  assert!(matches!(run_one_connection(&home, &gateway).await.unwrap(), ConnectionExit::Stopped { code: 4004 }));
  let identify = &gateway.received()[0][0];
  assert_eq!(identify["op"], 2);
  assert_eq!(identify["d"]["token"], TEST_TOKEN);
  assert_eq!(identify["d"]["intents"], 37377);
  assert_eq!(identify["d"]["properties"]["browser"], "charlie-bot");
  assert_eq!(identify["d"]["properties"]["device"], "charlie-bot");
}

#[tokio::test]
async fn test_heartbeat_carries_the_last_seq() {
  let home = Home::new(true).await;
  let mut script = GatewayScript::new(vec![ready(7)]);
  script.interval_ms = 15;
  script.acknowledge_heartbeats = true;
  // The heartbeat timer starts at HELLO, so an early heartbeat can carry null before READY is read.
  script.close_after_sequenced_heartbeat = Some(4004);
  let gateway = Gateway::start(vec![script]).await;
  configure_gateway(&home, &gateway, 1 << 18);
  let exit = tokio::time::timeout(Duration::from_secs(4), run_one_connection(&home, &gateway)).await.unwrap().unwrap();
  assert!(matches!(exit, ConnectionExit::Stopped { code: 4004 }));
  let received = gateway.received();
  let heartbeat = received[0].iter().find(|payload| payload["op"] == 1 && !payload["d"].is_null()).unwrap();
  assert_eq!(heartbeat, &json!({"op": 1, "d": 7}));
}

#[tokio::test]
async fn test_acked_heartbeats_keep_beating_one_interval_apart() {
  let home = Home::new(true).await;
  let mut script = GatewayScript::new(vec![ready(2)]);
  script.interval_ms = 15;
  script.acknowledge_heartbeats = true;
  script.close_after_heartbeats = Some((3, 4004));
  let gateway = Gateway::start(vec![script]).await;
  configure_gateway(&home, &gateway, 1 << 18);
  assert!(matches!(run_one_connection(&home, &gateway).await.unwrap(), ConnectionExit::Stopped { code: 4004 }));
  let received = gateway.received();
  let beats = received[0].iter().filter(|payload| payload["op"] == 1).count();
  assert!(beats >= 3);
  assert!(received[0].iter().all(|payload| payload["op"] != 8));
}

#[tokio::test]
async fn test_server_op1_is_answered_at_once() {
  let home = Home::new(true).await;
  let mut script = GatewayScript::new(vec![ready(7), json!({"op": 1})]);
  script.interval_ms = 3_600_000;
  script.acknowledge_heartbeats = true;
  script.close_after_heartbeats = Some((1, 4004));
  let gateway = Gateway::start(vec![script]).await;
  configure_gateway(&home, &gateway, 1 << 18);
  assert!(matches!(run_one_connection(&home, &gateway).await.unwrap(), ConnectionExit::Stopped { code: 4004 }));
  let received = gateway.received();
  assert!(received[0].iter().any(|payload| payload == &json!({"op": 1, "d": 7})));
}

#[tokio::test]
async fn test_missing_ack_closes_and_reconnects() {
  let home = Home::new(true).await;
  let mut missed = GatewayScript::new(vec![ready(2)]);
  missed.interval_ms = 10;
  let stop = GatewayScript { close_after_identify: Some(4004), ..GatewayScript::new(Vec::new()) };
  let gateway = Gateway::start(vec![missed, stop]).await;
  configure_gateway(&home, &gateway, 1 << 18);
  tokio::time::timeout(
    Duration::from_secs(4),
    run_listener_loop(Arc::clone(&home.features), discord_client(&home), TEST_TOKEN.to_string(), options(), settings()),
  )
  .await
  .unwrap()
  .unwrap();
  let received = gateway.received();
  assert_eq!(received.len(), 2);
  assert!(received[0].iter().any(|payload| payload["op"] == 1));
  assert!(received[0].iter().any(|payload| payload["close_code"] == 4000));
  assert!(received.iter().all(|connection| connection[0]["op"] == 2));
}

#[tokio::test]
async fn test_op7_and_op9_end_the_connection_and_reconnect() {
  for op in [7, 9] {
    let home = Home::new(true).await;
    let first = GatewayScript::new(vec![ready(2), json!({"op": op})]);
    let second = GatewayScript { close_after_identify: Some(4004), ..GatewayScript::new(Vec::new()) };
    let gateway = Gateway::start(vec![first, second]).await;
    configure_gateway(&home, &gateway, 1 << 18);
    let started = tokio::time::Instant::now();
    tokio::time::timeout(
      Duration::from_secs(4),
      run_listener_loop(
        Arc::clone(&home.features),
        discord_client(&home),
        TEST_TOKEN.to_string(),
        options(),
        settings(),
      ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(started.elapsed() < Duration::from_millis(1800), "READY resets the reconnect backoff");
    let received = gateway.received();
    assert_eq!(received.len(), 2, "gateway op {op} reconnects");
    assert!(received.iter().all(|connection| connection[0]["op"] == 2));
  }
}

#[tokio::test]
async fn test_stop_close_codes_stop_without_reconnecting() {
  for code in [4004, 4010, 4011, 4012, 4013, 4014] {
    let home = Home::new(true).await;
    let gateway =
      Gateway::start(vec![GatewayScript { close_after_identify: Some(code), ..GatewayScript::new(Vec::new()) }]).await;
    configure_gateway(&home, &gateway, 1 << 18);
    tokio::time::timeout(
      Duration::from_secs(2),
      run_listener_loop(
        Arc::clone(&home.features),
        discord_client(&home),
        TEST_TOKEN.to_string(),
        options(),
        settings(),
      ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(gateway.state.connections.load(Ordering::SeqCst), 1, "close code {code} stops the listener");
    assert!(stop_reason(code).is_some());
  }
}

#[tokio::test]
async fn test_ready_runs_the_backfill_per_connection() {
  let home = Home::new(true).await;
  let mut script = GatewayScript::new(vec![ready(2)]);
  script.close_after_identify = Some(4004);
  let gateway = Gateway::start(vec![script]).await;
  configure_gateway(&home, &gateway, 1 << 18);
  let sid = summon_session_id(GUILD, THREAD);
  home.thread_session(&sid, origin(THREAD), Some("500000000000000000")).await;
  home
    .fake()
    .channels
    .insert(THREAD.to_string(), vec![thread_message("500000000000000001", ALLOWED_USER, false, "synthetic follow")]);
  tokio::time::timeout(
    Duration::from_secs(2),
    run_listener_loop(Arc::clone(&home.features), discord_client(&home), TEST_TOKEN.to_string(), options(), settings()),
  )
  .await
  .unwrap()
  .unwrap();
  let manager = home.features.startup.require::<Arc<TriggerManager>>().unwrap();
  assert!(manager.list_triggers(&sid).await.unwrap().iter().any(|trigger| {
    trigger.status == TriggerStatus::Pending && trigger.message == "discord-thread-follow floor=500000000000000001"
  }));
}

#[tokio::test]
async fn test_message_create_reaches_the_handler_and_survives_its_errors() {
  let home = Home::new(true).await;
  let failed = json!({
    "id": "900000000000000060", "guild_id": GUILD, "channel_id": "900000000000000061",
    "author": {"id": ALLOWED_USER}, "type": 0, "content": "<@600000000000000001> ignored",
    "mentions": [{"id": BOT_USER}],
  });
  let dm = json!({
    "id": "900000000000000062", "channel_id": DM_CHANNEL, "author": {"id": ALLOWED_USER}, "type": 0,
    "content": "<@600000000000000001> hello", "mentions": [{"id": BOT_USER}],
  });
  let mut script = GatewayScript::new(vec![
    ready(2),
    gateway_dispatch("MESSAGE_CREATE", 3, failed),
    gateway_dispatch("MESSAGE_CREATE", 4, dm),
  ]);
  script.close_after_identify = Some(4004);
  let gateway = Gateway::start(vec![script]).await;
  configure_gateway(&home, &gateway, 1 << 18);
  tokio::time::timeout(
    Duration::from_secs(2),
    run_listener_loop(Arc::clone(&home.features), discord_client(&home), TEST_TOKEN.to_string(), options(), settings()),
  )
  .await
  .unwrap()
  .unwrap();
  assert!(home.fake().posts.iter().any(|(channel, content)| channel == DM_CHANNEL && content == DM_NOTICE));
}

#[tokio::test]
async fn test_preflight_intent_off_returns_before_connecting() {
  let home = Home::new(true).await;
  let gateway = Gateway::start(vec![GatewayScript::new(Vec::new())]).await;
  configure_gateway(&home, &gateway, 0);
  tokio::time::timeout(
    Duration::from_secs(1),
    run_listener_loop(Arc::clone(&home.features), discord_client(&home), TEST_TOKEN.to_string(), options(), settings()),
  )
  .await
  .unwrap()
  .unwrap();
  assert_eq!(gateway.state.connections.load(Ordering::SeqCst), 0);
  assert!(!message_content_intent_enabled(0));
}

#[tokio::test]
async fn test_preflight_logs_missing_permission_names_and_keeps_running() {
  let home = Home::new(true).await;
  let gateway =
    Gateway::start(vec![GatewayScript { close_after_identify: Some(4004), ..GatewayScript::new(Vec::new()) }]).await;
  configure_gateway(&home, &gateway, 1 << 18);
  let permissions = crate::discord_client::REQUIRED_PERMISSIONS
    .iter()
    .filter(|(name, _)| *name != "ADD_REACTIONS")
    .fold(0_u64, |bits, (_, bit)| bits | bit);
  home.fake().guilds = json!([{"id": GUILD, "name": "synthetic-guild", "permissions": permissions.to_string()}]);
  tokio::time::timeout(
    Duration::from_secs(2),
    run_listener_loop(Arc::clone(&home.features), discord_client(&home), TEST_TOKEN.to_string(), options(), settings()),
  )
  .await
  .unwrap()
  .unwrap();
  assert_eq!(gateway.state.connections.load(Ordering::SeqCst), 1);
  assert_eq!(crate::discord_client::missing_permissions(permissions), ["ADD_REACTIONS"]);
}

#[tokio::test]
async fn test_text_channel_summon_starts_thread_and_session() {
  let home = Home::new(true).await;
  home.fake().channel_objects.insert(PARENT.to_string(), json!({"id": PARENT, "type": 0, "name": "general"}));
  let sid = handle_message_create(&message(MENTION, PARENT), &home.features, discord_client(&home), BOT_USER)
    .await
    .unwrap()
    .unwrap();
  wait_for(|| home.fake().calls.iter().any(|call| call.contains("/reactions/"))).await;
  let meta = home.features.store.get_session(&sid).await.unwrap().unwrap();
  assert_eq!(sid, summon_session_id(GUILD, "900000000000000003"));
  assert_eq!(meta.extra["discord_origin"], origin("900000000000000003"));
  assert_eq!(meta.group.as_deref(), Some("Discord #general"));
  let events = home
    .features
    .store
    .load_chat_events_sync(&sid)
    .unwrap()
    .iter()
    .map(charliebot_store::ChatEvent::to_value)
    .collect::<Vec<_>>();
  let summon = events.iter().find(|event| event["type"] == event_types::AGENT_MESSAGE).unwrap();
  assert_eq!(summon["content"], DISCORD.summon_line);
  assert_eq!(
    summon["discord"],
    json!({"guild_id": GUILD, "channel_id": PARENT, "thread_id": "900000000000000003", "mention_id": MENTION})
  );
  let fake = home.fake();
  let calls = &fake.calls;
  assert!(calls.iter().any(|call| call.contains(&format!("/messages/{MENTION}/threads"))));
  assert!(calls.iter().any(|call| call.contains("PUT /api/v10/channels/") && call.contains("/reactions/")));
  assert_eq!(fake.started_threads, [(PARENT.to_string(), MENTION.to_string(), "plan the release".to_string())]);
}

#[test]
fn test_contract_part_names_the_read_command_and_the_summon_mark() {
  let contract = crate::thread_contract::thread_contract_part();
  assert!(contract.contains(READ_COMMAND));
  assert!(contract.contains("summon: true"));
  assert!(contract.contains(FOLLOW_TRIGGER_PREFIX));
  assert!(contract.contains("读线程里的新消息"));
}

#[test]
fn test_thread_name_keeps_the_thread_name_format() {
  assert_eq!(thread_name("a<@1>b"), "ab");
  assert_eq!(thread_name("<@١٢>x"), "x");
  assert_eq!(thread_name("hi\u{1c}<@1>there"), "hi there");
  assert_eq!(thread_name("<@1> <@!2>"), "CharlieBot");
  assert_eq!(thread_name(&format!("<@1>{}", "é".repeat(90))), "é".repeat(80));
}

#[tokio::test]
async fn test_thread_summon_binds_the_thread_and_labels_from_the_parent() {
  let home = Home::new(true).await;
  home
    .fake()
    .channel_objects
    .insert(THREAD.to_string(), json!({"id": THREAD, "type": 11, "name": "release-thread", "parent_id": PARENT}));
  home.fake().channel_objects.insert(PARENT.to_string(), json!({"id": PARENT, "type": 0, "name": "general"}));
  let sid = handle_message_create(&message(MENTION, THREAD), &home.features, discord_client(&home), BOT_USER)
    .await
    .unwrap()
    .unwrap();
  assert_eq!(sid, summon_session_id(GUILD, THREAD));
  let meta = home.features.store.get_session(&sid).await.unwrap().unwrap();
  assert_eq!(meta.extra["discord_origin"], origin(THREAD));
  let summon = home
    .features
    .store
    .load_chat_events_sync(&sid)
    .unwrap()
    .iter()
    .map(charliebot_store::ChatEvent::to_value)
    .find(|event| event["type"] == event_types::AGENT_MESSAGE)
    .unwrap();
  assert_eq!(summon["discord"]["channel_id"], THREAD);
  assert_eq!(summon["discord"]["thread_id"], THREAD);
  assert!(!home.fake().calls.iter().any(|call| call.contains("/threads")));
}

#[tokio::test]
async fn test_duplicate_summon_reuses_and_keeps_an_archived_session_archived() {
  let home = Home::new(true).await;
  home.fake().channel_objects.insert(PARENT.to_string(), json!({"id": PARENT, "type": 0, "name": "general"}));
  let client = discord_client(&home);
  let summon = message(MENTION, PARENT);
  let first = handle_message_create(&summon, &home.features, Arc::clone(&client), BOT_USER).await.unwrap().unwrap();
  let second = handle_message_create(&summon, &home.features, Arc::clone(&client), BOT_USER).await.unwrap().unwrap();
  home.features.store.archive_session(&first).await.unwrap();
  let third = handle_message_create(&summon, &home.features, client, BOT_USER).await.unwrap().unwrap();
  assert_eq!(first, second);
  assert_eq!(second, third);
  assert_eq!(home.features.store.get_session(&first).await.unwrap().unwrap().status, SessionStatus::Archived);
  assert_eq!(home.features.store.load_session_metas(None).unwrap().len(), 1);
  assert_eq!(
    home
      .features
      .store
      .load_chat_events_sync(&first)
      .unwrap()
      .iter()
      .filter(|event| event["type"] == event_types::AGENT_MESSAGE)
      .count(),
    1
  );
}

#[tokio::test]
async fn test_concurrent_duplicate_discord_message_create_accepts_one_thread_summon() {
  let home = Home::new(true).await;
  home
    .fake()
    .channel_objects
    .insert(THREAD.to_string(), json!({"id": THREAD, "type": 11, "name": "release-thread", "parent_id": PARENT}));
  home.fake().channel_objects.insert(PARENT.to_string(), json!({"id": PARENT, "type": 0, "name": "general"}));
  let client = discord_client(&home);
  let summon = message(MENTION, THREAD);
  let (first, second) = tokio::join!(
    handle_message_create(&summon, &home.features, Arc::clone(&client), BOT_USER),
    handle_message_create(&summon, &home.features, Arc::clone(&client), BOT_USER),
  );
  let first = first.unwrap().unwrap();
  let second = second.unwrap().unwrap();

  assert_eq!(first, second);
  let events = home.features.store.load_chat_events_sync(&first).unwrap();
  let summons: Vec<_> = events
    .iter()
    .filter(|event| event["type"] == event_types::AGENT_MESSAGE && event.get("discord").is_some())
    .collect();
  assert_eq!(summons.len(), 1);
  assert_eq!(summons[0]["request_id"], format!("discord-mention:{MENTION}"));

  wait_for(|| home.fake().calls.iter().any(|call| call.contains("/reactions/"))).await;
  let fake = home.fake();
  assert_eq!(fake.calls.iter().filter(|call| call.contains("/reactions/")).count(), 1);
  assert!(fake.started_threads.is_empty());
}

#[tokio::test]
async fn test_discord_accepted_post_timeout_retries_with_one_nonce_keyed_post() {
  let home = Home::new(true).await;
  let sid = discord_session(&home).await;
  home.fake().accepted_post_failures_remaining = 1;
  let bot = Bot::new(Arc::clone(&home.features), home.api.clone());

  let result = post_reply(&bot, &sid, "accepted before the response failed").await.unwrap();

  assert_eq!(result["posted"], true);
  let fake = home.fake();
  assert_eq!(fake.posts, [(THREAD.to_string(), "accepted before the response failed".to_string())]);
  assert_eq!(fake.post_bodies.len(), 2);
  assert_eq!(fake.post_bodies[0]["nonce"], fake.post_bodies[1]["nonce"]);
  assert_eq!(fake.post_bodies[0]["enforce_nonce"], true);
  assert_eq!(fake.post_bodies[1]["enforce_nonce"], true);
}

#[tokio::test]
async fn test_existing_bot_content_blocks_a_post_without_nonce_deduplication() {
  let home = Home::new(true).await;
  let sid = discord_session(&home).await;
  let text = "the reply already present in the thread";
  home.fake().channels.insert(
    THREAD.to_string(),
    vec![json!({
      "id": "900000000000000101",
      "channel_id": THREAD,
      "content": text,
      "author": {"id": BOT_USER, "username": "synthetic-bot", "bot": true},
      "timestamp": "2099-01-01T00:00:00Z",
    })],
  );
  let bot = Bot::new(Arc::clone(&home.features), home.api.clone());

  let result = post_reply(&bot, &sid, text).await.unwrap();

  assert_eq!(result["posted"], true);
  assert!(home.fake().posts.is_empty());
}

#[tokio::test]
async fn test_disallowed_user_and_bot_author_create_nothing() {
  let home = Home::new(true).await;
  let client = discord_client(&home);
  let disallowed = json!({
    "id": MENTION, "guild_id": GUILD, "channel_id": PARENT, "author": {"id": OTHER_USER},
    "type": 0, "content": "<@600000000000000001> hi", "mentions": [{"id": BOT_USER}],
  });
  let bot = json!({
    "id": MENTION, "guild_id": GUILD, "channel_id": PARENT, "author": {"id": ALLOWED_USER, "bot": true},
    "type": 0, "content": "<@600000000000000001> hi", "mentions": [{"id": BOT_USER}],
  });
  assert!(handle_message_create(&disallowed, &home.features, Arc::clone(&client), BOT_USER).await.unwrap().is_none());
  assert!(handle_message_create(&bot, &home.features, client, BOT_USER).await.unwrap().is_none());
  assert!(home.fake().calls.is_empty());
  assert!(home.features.store.load_session_metas(None).unwrap().is_empty());
}

#[tokio::test]
async fn test_allowed_dm_mention_gets_the_notice_only() {
  let home = Home::new(true).await;
  let mut payload = message(MENTION, DM_CHANNEL);
  payload.as_object_mut().unwrap().remove("guild_id");
  assert!(handle_message_create(&payload, &home.features, discord_client(&home), BOT_USER).await.unwrap().is_none());
  assert_eq!(home.fake().posts, [(DM_CHANNEL.to_string(), DM_NOTICE.to_string())]);
  assert!(home.features.store.load_session_metas(None).unwrap().is_empty());
}

#[tokio::test]
async fn test_unmentioned_message_arms_follow_and_compares_ids_as_integers() {
  let home = Home::new(true).await;
  let sid = summon_session_id(GUILD, THREAD);
  home.thread_session(&sid, origin(THREAD), Some("999")).await;
  let mut payload = message("1000000000000000100", THREAD);
  payload["mentions"] = json!([]);
  payload["content"] = json!("the follow-up");
  assert_eq!(
    handle_message_create(&payload, &home.features, discord_client(&home), BOT_USER).await.unwrap(),
    Some(sid.clone())
  );
  let manager = home.features.startup.require::<Arc<TriggerManager>>().unwrap();
  let triggers = manager.list_triggers(&sid).await.unwrap();
  let follow = triggers.iter().find(|trigger| trigger.status == TriggerStatus::Pending).unwrap();
  assert_eq!(follow.message, "discord-thread-follow floor=1000000000000000100");
}

#[tokio::test]
async fn test_archived_session_revives_and_arms_on_an_unmentioned_message() {
  let home = Home::new(true).await;
  let sid = summon_session_id(GUILD, THREAD);
  home.thread_session(&sid, origin(THREAD), None).await;
  home.features.store.archive_session(&sid).await.unwrap();
  let mut payload = message("1000000000000000100", THREAD);
  payload["mentions"] = json!([]);
  payload["content"] = json!("the follow-up");
  assert_eq!(
    handle_message_create(&payload, &home.features, discord_client(&home), BOT_USER).await.unwrap(),
    Some(sid.clone())
  );
  assert_eq!(home.features.store.get_session(&sid).await.unwrap().unwrap().status, SessionStatus::Active);
  let manager = home.features.startup.require::<Arc<TriggerManager>>().unwrap();
  assert!(
    manager.list_triggers(&sid).await.unwrap().iter().any(|trigger| trigger.message.starts_with(FOLLOW_TRIGGER_PREFIX))
  );
}

#[tokio::test]
async fn test_read_eligible_pages_two_calls_and_drops_bots() {
  let home = Home::new(true).await;
  let humans: Vec<Value> =
    (0..101).map(|index| thread_message(&format!("500000000000000{index:03}"), ALLOWED_USER, false, "human")).collect();
  let mut messages = humans.clone();
  messages.push(thread_message("500000000000000500", OTHER_USER, true, "noise"));
  messages.sort_by_key(|message| crate::discord_client::snowflake_key(message["id"].as_str().unwrap()));
  home.fake().channels.insert(THREAD.to_string(), messages);
  let adapter = DiscordThreadAdapter::with_client(
    Bot::new(Arc::clone(&home.features), home.api.clone()),
    (*discord_client(&home)).clone(),
  );
  let config = home.features.config.get().unwrap();
  let eligible = adapter.read_eligible(&origin(THREAD), &config).await.unwrap();
  assert_eq!(eligible.len(), 101);
  assert!(eligible.iter().all(|message| message.user.as_deref() == Some(ALLOWED_USER)));
  let fake = home.fake();
  let calls = &fake.calls;
  assert_eq!(calls.iter().filter(|call| call.contains("/messages?")).count(), 2);
}

async fn discord_session(home: &Home) -> String {
  let sid = summon_session_id(GUILD, THREAD);
  home.thread_session(&sid, origin(THREAD), None).await;
  sid
}

#[tokio::test]
async fn test_post_reply_posts_a_published_url_byte_identical() {
  let home = Home::new(true).await;
  let sid = discord_session(&home).await;
  let text = "see https://example.test/Ab3dEf6hIj8kLm1nOp2q/page.html for details";
  let bot = Bot::new(Arc::clone(&home.features), home.api.clone());
  let readback = post_reply(&bot, &sid, text).await.unwrap();
  assert_eq!(readback["text"], text);
  assert_eq!(home.fake().posts[0], (THREAD.to_string(), text.to_string()));
}

#[tokio::test]
async fn test_post_reply_names_an_application_route_link_for_the_operator() {
  let home = Home::new(true).await;
  let sid = discord_session(&home).await;
  let port = home.features.config.get().unwrap().server.port;
  let url = format!("http://127.0.0.1:{port}/diff");
  let bot = Bot::new(Arc::clone(&home.features), home.api.clone());
  let readback = post_reply(&bot, &sid, &format!("open {url}")).await.unwrap();
  assert!(readback["operator_only_note"].as_str().unwrap().contains(&url));
}

#[tokio::test]
async fn test_post_reply_refuses_422_and_posts_nothing_on_a_file_server_link() {
  let home = Home::new(true).await;
  let sid = discord_session(&home).await;
  let bot = Bot::new(Arc::clone(&home.features), home.api.clone());
  for link in [
    "http://localhost:18650/absolute_filepath/home/u/artifacts/page.html",
    "https://charliebot.example/absolute_filepath/home/u/artifacts/page.html",
    "https://charliebot.example/absolute_filepath/home/u/caf%C3%A9/page.html?q=1#frag",
  ] {
    let error = post_reply(&bot, &sid, &format!("see {link} for details")).await.unwrap_err();
    assert_eq!(error.status.as_u16(), 422);
    assert!(error.to_string().contains(link));
    assert!(error.to_string().contains("charliebot publish"));
  }
  assert!(home.fake().posts.is_empty());
  assert!(
    home
      .features
      .store
      .load_chat_events_sync(&sid)
      .unwrap()
      .iter()
      .all(|event| event.get("type").and_then(charliebot_store::compact::CompactValue::as_str)
        != Some(crate::event_types::DISCORD_REPLY))
  );
}

#[tokio::test]
async fn test_after_turn_audits_the_round_on_the_bound_blocks_and_persists_the_nudge() {
  let home = Home::new(true).await;
  let sid = discord_session(&home).await;
  let block = json!({"guild_id": GUILD, "channel_id": PARENT, "thread_id": THREAD, "mention_id": MENTION});
  let summon = home
    .features
    .store
    .append_chat_event(
      &sid,
      json!({"type": event_types::AGENT_MESSAGE, "content": DISCORD.summon_line, "discord": block}),
    )
    .await
    .unwrap()
    .to_value();
  let done = json!({"type": event_types::MASTER_DONE, "exit_code": 0, "input_event_id": summon["id"]});
  home.features.store.append_chat_event(&sid, done.clone()).await.unwrap();
  let meta = home.features.store.get_session(&sid).await.unwrap().unwrap();
  let config = home.features.config.get().unwrap();
  crate::turn_contribution::DiscordTurnContribution.after_turn(&meta, &done, &config, &home.features).await.unwrap();
  let nudge = home
    .features
    .store
    .load_chat_events_sync(&sid)
    .unwrap()
    .iter()
    .map(charliebot_store::ChatEvent::to_value)
    .find(|event| event["type"] == event_types::AGENT_MESSAGE && event["discord"].get("nudge_of").is_some())
    .unwrap();
  assert_eq!(
    nudge["discord"],
    json!({
      "guild_id": GUILD,
      "channel_id": PARENT,
      "thread_id": THREAD,
      "mention_id": MENTION,
      "nudge_of": summon["id"],
    })
  );
}

#[tokio::test]
async fn test_persisted_master_done_fires_both_deliver_tasks() {
  let home = Home::new(true).await;
  let sid = discord_session(&home).await;
  let block = json!({"guild_id": GUILD, "channel_id": PARENT, "thread_id": THREAD, "mention_id": MENTION});
  let summon = home
    .append(&sid, json!({"type": event_types::AGENT_MESSAGE, "content": DISCORD.summon_line, "discord": block}))
    .await;
  let done = json!({"type": event_types::MASTER_DONE, "exit_code": 0, "input_event_ids": [summon["id"]]});
  home.features.store.persist_and_broadcast(&sid, done).await.unwrap();
  wait_for(|| {
    home.features.store.load_chat_events_sync(&sid).unwrap().iter().any(|event| {
      event.get("type").and_then(charliebot_store::compact::CompactValue::as_str) == Some(event_types::AGENT_MESSAGE)
        && event.get("discord").and_then(|block| block.get("nudge_of")).is_some()
    })
  })
  .await;
  let events = home
    .features
    .store
    .load_chat_events_sync(&sid)
    .unwrap()
    .iter()
    .map(charliebot_store::ChatEvent::to_value)
    .collect::<Vec<_>>();
  let nudge = events
    .iter()
    .find(|event| event["type"] == event_types::AGENT_MESSAGE && event["discord"].get("nudge_of").is_some())
    .unwrap();
  assert_eq!(
    nudge["discord"],
    json!({
      "guild_id": GUILD,
      "channel_id": PARENT,
      "thread_id": THREAD,
      "mention_id": MENTION,
      "nudge_of": summon["id"],
    })
  );
  assert_eq!(nudge["type"], event_types::AGENT_MESSAGE);
}

#[tokio::test]
async fn test_deliver_done_skips_a_session_without_discord_origin() {
  let home = Home::new(false).await;
  let sid = home.plain_session("plain-session").await;
  let before = home.features.store.load_chat_events_sync(&sid).unwrap();
  let done = json!({"type": event_types::MASTER_DONE, "input_event_ids": ["e1"]});
  assert!(!deliver_done(&home.features, &sid, &done).await.unwrap());
  assert_eq!(home.features.store.load_chat_events_sync(&sid).unwrap(), before);
}

#[test]
fn the_discord_platform_name_is_a_thread_platform_key() {
  // Python: tests/test_run_token.py:243.
  assert!(event_types::THREAD_PLATFORM_KEYS.contains(&DISCORD.name));
}
