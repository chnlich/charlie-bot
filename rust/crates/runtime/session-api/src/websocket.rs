use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use axum::response::Response;
use charliebot_registry::{FeatureContext, WebSocketRequest};
use charliebot_store::aggregator::MessageAggregator;
use charliebot_store::metadata::{SessionMetadata, TaskProfile};
use charliebot_store::streaming::{self, SocketHandle, Subscription};
use charliebot_store::{RAW_EVENTS_REPLACED_BY_DELTAS, SharedChatEvent, Store, StoreError, StoreResult};
use futures_util::future::BoxFuture;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::time;

use crate::route_support::blocking_store;

const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(30);
const CATCHUP_SLICE_EVENTS: usize = 400;

pub(crate) fn session_websocket(
  features: Arc<FeatureContext>,
  request: WebSocketRequest,
) -> BoxFuture<'static, Response> {
  let WebSocketRequest { segments, query, upgrade, .. } = request;
  let session_id = segments.into_iter().next().expect("the session WebSocket route captures its session id");
  Box::pin(async move {
    upgrade.on_upgrade(move |socket| async move { serve_session_socket(features, session_id, query, socket).await })
  })
}

async fn serve_session_socket(features: Arc<FeatureContext>, session_id: String, query: String, socket: WebSocket) {
  let (mut sender, mut receiver) = socket.split();
  tracing::info!(session_id, "session_ws_connected");

  // The replay cursor rides the handshake query (`server.py` `session_websocket`): an absent, overlong or
  // non-decimal value closes with 4400 before the subscription starts.
  let raw_cursor = raw_cursor(&query);
  let cursor = match parse_handshake_cursor(raw_cursor.as_deref()) {
    Some(cursor) => cursor,
    None => {
      tracing::warn!(session_id, raw_cursor = ?raw_cursor, "session_ws_cursor_invalid");
      if let Err(close_error) = close_with_code(&mut sender, 4400, "").await {
        tracing::info!(session_id, reason = %close_error, "session_ws_closed");
      }
      return;
    }
  };

  let mut socket = features.store.register_socket();
  let store = Arc::clone(&features.store);
  let load_store = Arc::clone(&store);
  let load_session_id = session_id.clone();
  let (mut session_subscription, mut sidebar_subscription, catchup_result) =
    subscribe_before_load(&store, &session_id, || async move {
      let meta = load_store.get_session(&load_session_id).await?;
      Ok::<_, StoreError>(prepare_catchup(load_store, load_session_id, cursor, meta).await)
    })
    .await;

  match catchup_result {
    Err(error) => {
      tracing::error!(session_id, error = %error, "session_ws_catchup_failed");
      if let Err(close_error) = close_with_code(&mut sender, 1011, "").await {
        tracing::info!(session_id, reason = %close_error, "session_ws_closed");
      }
      features.store.unsubscribe(&mut session_subscription);
      features.store.unsubscribe(&mut sidebar_subscription);
      tracing::info!(session_id, "session_ws_disconnected");
      return;
    }
    Ok(Err(error)) => {
      tracing::warn!(session_id, error = %error, "session_ws_catchup_failed");
    }
    Ok(Ok(catchup)) => {
      let sent = send_catchup_frames(&mut sender, &catchup.frames, &session_id).await;
      match sender.send(Message::Text(json!({"type": "catchup_complete"}).to_string().into())).await {
        Ok(()) => tracing::debug!(session_id, cursor, total = catchup.total, sent, "session_ws_catchup_sent"),
        Err(error) => tracing::warn!(session_id, error = %error, "session_ws_catchup_failed"),
      }
    }
  }

  keepalive(&mut sender, &mut receiver, &mut session_subscription, &mut sidebar_subscription, &mut socket, &session_id)
    .await;
  features.store.unsubscribe(&mut session_subscription);
  features.store.unsubscribe(&mut sidebar_subscription);
  tracing::info!(session_id, "session_ws_disconnected");
}

async fn subscribe_before_load<T, F, Fut>(store: &Store, session_id: &str, load: F) -> (Subscription, Subscription, T)
where
  F: FnOnce() -> Fut,
  Fut: Future<Output = T>,
{
  let session_subscription = store.subscribe(&streaming::session_channel(session_id));
  let sidebar_subscription = store.subscribe(streaming::SIDEBAR_CHANNEL);
  let loaded = load().await;
  (session_subscription, sidebar_subscription, loaded)
}

/// The raw `cursor` value of the handshake query, or `None` when the query has no `cursor`. Repeated keys
/// read the first value, the way Starlette's `query_params.get` does.
fn raw_cursor(query: &str) -> Option<String> {
  form_urlencoded::parse(query.as_bytes()).find(|(key, _)| key == "cursor").map(|(_, value)| value.into_owned())
}

/// The replay cursor the handshake check accepts (`server.py` `session_websocket`): the value is present,
/// at most 19 characters, ASCII and decimal. `int` then parses it without a digit-limit error, and a
/// 19-digit value above `i64::MAX` stays valid and lies past every event total.
fn parse_handshake_cursor(raw: Option<&str>) -> Option<i128> {
  let raw = raw?;
  if raw.is_empty() || raw.len() > 19 || !raw.is_ascii() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
    return None;
  }
  Some(raw.parse().expect("at most 19 ASCII decimal digits fit i128"))
}

struct Catchup {
  frames: Vec<Value>,
  total: usize,
}

async fn prepare_catchup(
  store: Arc<Store>,
  session_id: String,
  cursor: i128,
  meta: Option<SessionMetadata>,
) -> StoreResult<Catchup> {
  if meta.as_ref().is_some_and(|meta| meta.profile == TaskProfile::Worker) {
    let worker_store = Arc::clone(&store);
    let worker_session_id = session_id.clone();
    let events = blocking_store(move || {
      Ok(Arc::clone(&crate::worker_transcript::load_worker_transcript(&worker_store, &worker_session_id)?.events))
    })
    .await?;
    let total = events.len();
    let frames = replay_events(&store, events, cursor, 0).await?;
    return Ok(Catchup { frames, total });
  }

  let event_index_offset = meta.as_ref().map_or(0, |meta| meta.archive_offset.max(0) as usize);
  let count_store = Arc::clone(&store);
  let count_session_id = session_id.clone();
  let event_count = match blocking_store(move || count_store.get_chat_event_count_sync(&count_session_id)).await {
    Ok(total) => Some(total),
    Err(error) => {
      tracing::warn!(session_id, error = %error, "session_ws_event_count_failed");
      None
    }
  };
  if event_count.is_some_and(|total| cursor >= total as i128) {
    return Ok(Catchup { frames: Vec::new(), total: event_count.expect("the event count was checked above") });
  }

  let events_store = Arc::clone(&store);
  let events_session_id = session_id.clone();
  let events = blocking_store(move || events_store.load_chat_events_shared_sync(&events_session_id)).await?;
  let total = event_count.unwrap_or(event_index_offset + events.len());
  let frames = replay_events(&store, events, cursor, event_index_offset).await?;
  Ok(Catchup { frames, total })
}

trait CatchupEvent {
  /// The event as the `Value` form the catch-up aggregator and frames read.
  fn event(&self) -> Value;
}

impl CatchupEvent for Value {
  fn event(&self) -> Value {
    self.clone()
  }
}

impl CatchupEvent for SharedChatEvent {
  fn event(&self) -> Value {
    self.as_ref().to_value()
  }
}

async fn replay_events<E: CatchupEvent + Send + Sync>(
  store: &Store,
  events: Arc<Vec<E>>,
  cursor: i128,
  event_index_offset: usize,
) -> StoreResult<Vec<Value>> {
  let mut walk = CatchupWalk::new(store.new_message_aggregator(event_index_offset, true), cursor, event_index_offset);
  for start in (0..events.len()).step_by(CATCHUP_SLICE_EVENTS) {
    let end = (start + CATCHUP_SLICE_EVENTS).min(events.len());
    walk.feed_slice(events.as_slice(), start, end)?;
    tokio::task::yield_now().await;
  }
  Ok(walk.finish())
}

struct CatchupWalk {
  aggregator: MessageAggregator,
  cursor: i128,
  event_index_offset: usize,
  frames: Vec<Value>,
  latest_stream: Option<Value>,
}

impl CatchupWalk {
  fn new(aggregator: MessageAggregator, cursor: i128, event_index_offset: usize) -> Self {
    Self { aggregator, cursor, event_index_offset, frames: Vec::new(), latest_stream: None }
  }

  fn feed_slice<E: CatchupEvent>(&mut self, events: &[E], start: usize, end: usize) -> StoreResult<()> {
    for (index, event) in events.iter().enumerate().take(end).skip(start) {
      let global_index = self.event_index_offset + index;
      let event = event.event();
      let deltas = self.aggregator.feed(&event)?;
      if (global_index as i128) < self.cursor {
        continue;
      }
      for delta in deltas {
        if delta.get("type").and_then(Value::as_str) == Some("stream") {
          self.latest_stream = Some(delta);
        } else {
          self.latest_stream = None;
          self.frames.push(delta);
        }
      }
      let event_type = event.get("type").and_then(Value::as_str);
      if event_type.is_some_and(|event_type| RAW_EVENTS_REPLACED_BY_DELTAS.contains(&event_type)) {
        continue;
      }
      let mut payload = event.clone();
      payload
        .as_object_mut()
        .expect("the message aggregator accepts only event objects")
        .entry("event_index".to_string())
        .or_insert_with(|| json!(global_index));
      self.frames.push(payload);
    }
    Ok(())
  }

  fn finish(mut self) -> Vec<Value> {
    if let Some(stream) = self.latest_stream {
      self.frames.push(stream);
    }
    self.frames
  }
}

async fn send_catchup_frames(
  sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
  frames: &[Value],
  session_id: &str,
) -> usize {
  let mut sent = 0;
  for frame in frames {
    let text = serde_json::to_string(frame).expect("JSON values serialize to WebSocket text");
    if let Err(error) = sender.send(Message::Text(text.into())).await {
      tracing::debug!(session_id, error = %error, "session_ws_catchup_send_failed");
      return sent;
    }
    sent += 1;
  }
  sent
}

async fn keepalive(
  sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
  receiver: &mut futures_util::stream::SplitStream<WebSocket>,
  session_subscription: &mut Subscription,
  sidebar_subscription: &mut Subscription,
  socket: &mut SocketHandle,
  session_id: &str,
) {
  let keepalive = time::sleep(KEEPALIVE_TIMEOUT);
  tokio::pin!(keepalive);
  loop {
    tokio::select! {
      incoming = receiver.next() => match incoming {
        Some(Ok(Message::Text(_))) => keepalive.as_mut().reset(time::Instant::now() + KEEPALIVE_TIMEOUT),
        Some(Ok(Message::Close(_))) | None => return,
        Some(Ok(Message::Binary(_))) => {
          tracing::info!(session_id, reason = "binary frame received", "session_ws_closed");
          return;
        }
        Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
        Some(Err(error)) => {
          tracing::info!(session_id, reason = %error, "session_ws_closed");
          return;
        }
      },
      frame = session_subscription.recv() => match frame {
        Some(frame) => if let Err(error) = sender.send(Message::Text(frame.into())).await {
          tracing::info!(session_id, reason = %error, "session_ws_closed");
          return;
        },
        None => unreachable!("the store keeps a subscriber's sender until this socket unsubscribes"),
      },
      frame = sidebar_subscription.recv() => match frame {
        Some(frame) => if let Err(error) = sender.send(Message::Text(frame.into())).await {
          tracing::info!(session_id, reason = %error, "session_ws_closed");
          return;
        },
        None => unreachable!("the store keeps a subscriber's sender until this socket unsubscribes"),
      },
      request = socket.recv_close_request() => match request {
        Some(reply) => {
          let outcome = close_with_code(sender, 1000, "").await.map_err(|error| error.to_string());
          // The shutdown waits on this reply. If it has stopped waiting, no one reads the outcome.
          let _ = reply.send(outcome);
          return;
        }
        None => unreachable!("the store keeps a socket's request sender until its handle drops"),
      },
      _ = &mut keepalive => {
        let ping = json!({"type": "ping"}).to_string();
        if let Err(error) = sender.send(Message::Text(ping.into())).await {
          tracing::info!(session_id, reason = %error, "session_ws_closed");
          return;
        }
        keepalive.as_mut().reset(time::Instant::now() + KEEPALIVE_TIMEOUT);
      }
    }
  }
}

async fn close_with_code(
  sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
  code: u16,
  reason: &str,
) -> Result<(), axum::Error> {
  let close = Message::Close(Some(CloseFrame { code, reason: reason.to_string().into() }));
  sender.send(close).await
}

#[cfg(test)]
mod tests {
  use std::net::SocketAddr;
  use std::sync::Arc;

  use axum::Router;
  use axum::extract::ws::WebSocketUpgrade;
  use axum::extract::{Path, RawQuery, State};
  use axum::routing::get;
  use charliebot_core::config::CREDENTIALS_FILENAME;
  use charliebot_core::credentials::load_credentials;
  use charliebot_core::hot_reload::HotReload;
  use charliebot_registry::store_hooks;
  use charliebot_registry::{FeatureContext, Registry, StartupState, WebSocketRequest};
  use charliebot_store::metadata::{SessionMetadata, TaskProfile};
  use charliebot_store::run_records::RunRecord;
  use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
  use tokio::net::{TcpListener, TcpStream};
  use tokio::time::{self, Duration};

  use super::*;

  fn test_context() -> (tempfile::TempDir, Arc<FeatureContext>) {
    let home = tempfile::tempdir().expect("temporary home");
    std::fs::write(home.path().join(CREDENTIALS_FILENAME), "charliebot: {}\n").expect("credentials file");
    let registry = Arc::new(Registry::new());
    let config_value = registry.load_config(home.path()).expect("test config");
    let store = Arc::new(
      Store::new(config_value.clone(), registry.metadata_slots(), store_hooks::store_hooks(Arc::clone(&registry)))
        .expect("test store"),
    );
    let config_registry = Arc::clone(&registry);
    let config_home = home.path().to_path_buf();
    let config = Arc::new(HotReload::new(
      config_value.config_file(),
      "session_ws_test_config_reload_failed",
      Box::new(move || config_registry.load_config(&config_home)),
    ));
    let credential_home = home.path().to_path_buf();
    let credentials = Arc::new(HotReload::new(
      home.path().join(CREDENTIALS_FILENAME),
      "session_ws_test_credentials_reload_failed",
      Box::new(move || load_credentials(&credential_home)),
    ));
    let startup = Arc::new(StartupState::default());
    (home, Arc::new(FeatureContext { config, credentials, store, registry, startup }))
  }

  fn write_session(features: &FeatureContext, session_id: &str, profile: TaskProfile, archive_offset: i64) {
    let mut metadata = SessionMetadata::new(session_id, "Synthetic session", profile);
    metadata.archive_offset = archive_offset;
    let path = features.store.metadata_path(session_id);
    std::fs::create_dir_all(path.parent().expect("metadata path has a parent")).expect("session directory");
    std::fs::write(path, metadata.to_json_string(&features.registry.metadata_slots(), true)).expect("session metadata");
  }

  async fn seed_manager_session(features: &FeatureContext, session_id: &str, archive_offset: i64) {
    write_session(features, session_id, TaskProfile::Manager, archive_offset);
    features
      .store
      .append_chat_event(session_id, json!({"type": "user", "content": "question"}))
      .await
      .expect("append user event");
    features
      .store
      .append_chat_event(
        session_id,
        json!({
          "type": "assistant",
          "message": {"content": [{"type": "text", "text": "draft"}]},
        }),
      )
      .await
      .expect("append assistant event");
    features.store.append_chat_event(session_id, json!({"type": "task_created"})).await.expect("append raw event");
  }

  #[test]
  fn handshake_cursor_query_cases_follow_python() {
    // Python accepts a non-empty run of at most 19 ASCII decimal digits (`server.py` `session_websocket`),
    // so leading zeros stay valid and a 19-digit value above i64::MAX parses.
    assert_eq!(parse_handshake_cursor(Some("9")), Some(9));
    assert_eq!(parse_handshake_cursor(Some("007")), Some(7));
    assert_eq!(parse_handshake_cursor(Some("9999999999999999999")), Some(9_999_999_999_999_999_999));
    assert_eq!(parse_handshake_cursor(None), None);
    assert_eq!(parse_handshake_cursor(Some("")), None);
    assert_eq!(parse_handshake_cursor(Some("-1")), None);
    assert_eq!(parse_handshake_cursor(Some("abc")), None);
    assert_eq!(parse_handshake_cursor(Some("9".repeat(20).as_str())), None);
  }

  #[test]
  fn raw_cursor_reads_the_first_cursor_pair_of_the_query() {
    assert_eq!(raw_cursor("cursor=9").as_deref(), Some("9"));
    assert_eq!(raw_cursor("token=k-1&cursor=12").as_deref(), Some("12"));
    assert_eq!(raw_cursor("cursor=4&cursor=12").as_deref(), Some("4"));
    assert_eq!(raw_cursor(""), None);
  }

  #[tokio::test]
  async fn cursors_at_and_past_the_total_send_no_catchup_frames() {
    let (_home, features) = test_context();
    seed_manager_session(&features, "synthetic", 0).await;
    let meta = features.store.get_session("synthetic").await.unwrap();
    let at_total =
      prepare_catchup(Arc::clone(&features.store), "synthetic".to_string(), 3, meta.clone()).await.unwrap();
    let past_total = prepare_catchup(Arc::clone(&features.store), "synthetic".to_string(), 4, meta).await.unwrap();
    assert_eq!(at_total.total, 3);
    assert!(at_total.frames.is_empty());
    assert_eq!(past_total.total, 3);
    assert!(past_total.frames.is_empty());
  }

  #[tokio::test]
  async fn catchup_replays_cursor_zero_and_drops_frames_before_a_middle_cursor() {
    let (_home, features) = test_context();
    seed_manager_session(&features, "synthetic", 0).await;
    let meta = features.store.get_session("synthetic").await.unwrap();
    let from_zero =
      prepare_catchup(Arc::clone(&features.store), "synthetic".to_string(), 0, meta.clone()).await.unwrap();
    let from_middle = prepare_catchup(Arc::clone(&features.store), "synthetic".to_string(), 1, meta).await.unwrap();
    let zero_types: Vec<&str> = from_zero.frames.iter().map(|frame| frame["type"].as_str().unwrap()).collect();
    let middle_types: Vec<&str> = from_middle.frames.iter().map(|frame| frame["type"].as_str().unwrap()).collect();
    assert_eq!(zero_types, ["message", "task_created", "stream"]);
    assert_eq!(middle_types, ["task_created", "stream"]);
    assert_eq!(from_middle.frames[0]["event_index"], 2);
  }

  #[tokio::test]
  async fn archived_catchup_uses_global_event_indexes() {
    let (_home, features) = test_context();
    seed_manager_session(&features, "synthetic", 7).await;
    let meta = features.store.get_session("synthetic").await.unwrap();
    let catchup = prepare_catchup(Arc::clone(&features.store), "synthetic".to_string(), 8, meta).await.unwrap();
    assert_eq!(catchup.total, 10);
    let types: Vec<&str> = catchup.frames.iter().map(|frame| frame["type"].as_str().unwrap()).collect();
    assert_eq!(types, ["task_created", "stream"]);
    assert_eq!(catchup.frames[0]["event_index"], 9);
    assert_eq!(catchup.frames[1]["message"]["event_index"], 8);
  }

  #[tokio::test]
  async fn worker_catchup_replays_the_projected_run_transcript_from_zero() {
    let (_home, features) = test_context();
    write_session(&features, "worker-synthetic", TaskProfile::Worker, 500);
    let run: RunRecord = serde_json::from_value(json!({
      "id": "run-synthetic",
      "session_id": "worker-synthetic",
      "kind": "work",
      "backend": "synthetic",
    }))
    .unwrap();
    let guard = features.store.control_lock().await;
    features.store.register_run_locked(&guard, "worker-synthetic", run, None).await.unwrap();
    drop(guard);
    let run_events = features.store.run_dir("worker-synthetic", "run-synthetic").join("events.jsonl");
    std::fs::write(
      run_events,
      concat!(
        "{\"type\":\"user\",\"content\":\"worker question\"}\n",
        "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"worker answer\"}]}}\n"
      ),
    )
    .unwrap();

    let meta = features.store.get_session("worker-synthetic").await.unwrap();
    let catchup = prepare_catchup(Arc::clone(&features.store), "worker-synthetic".to_string(), 0, meta).await.unwrap();
    assert_eq!(catchup.total, 3);
    assert_eq!(catchup.frames[0]["type"], "message");
    assert_eq!(catchup.frames[1]["type"], "run_header");
    assert_eq!(catchup.frames[2]["message"]["content"], "worker question");
    assert_eq!(catchup.frames.last().unwrap()["type"], "stream");
    assert_eq!(catchup.frames.last().unwrap()["message"]["content"], "worker answer");
    assert_eq!(catchup.frames.last().unwrap()["message"]["event_index"], 2);
  }

  #[tokio::test]
  async fn the_latest_stream_delta_is_buffered_at_the_end_of_catchup() {
    let (_home, features) = test_context();
    let events: Vec<SharedChatEvent> = vec![json!({
      "type": "assistant",
      "message": {"content": [{"type": "text", "text": "draft"}]},
    })]
    .into_iter()
    .map(|event| SharedChatEvent::new(charliebot_store::ChatEvent::from_value(&event)))
    .collect();
    let frames = replay_events(&features.store, Arc::new(events), 0, 0).await.unwrap();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0]["type"], "stream");
    assert_eq!(frames[0]["message"]["content"], "draft");
  }

  #[tokio::test]
  async fn subscriptions_exist_before_the_catchup_read_starts() {
    let (_home, features) = test_context();
    write_session(&features, "synthetic", TaskProfile::Manager, 0);
    let store = Arc::clone(&features.store);
    let (mut session, mut sidebar, events) = subscribe_before_load(&store, "synthetic", || async {
      store.broadcast_only("synthetic", json!({"type": "session_renamed", "name": "Synthetic"})).await.unwrap();
      store
        .broadcast_sidebar(
          "synthetic",
          "unread_changed",
          serde_json::Map::from_iter([("has_unread".to_string(), json!(true))]),
        )
        .await
        .unwrap();
      store.load_chat_events_sync("synthetic").unwrap()
    })
    .await;
    assert!(events.is_empty());
    assert_eq!(serde_json::from_str::<Value>(&session.recv().await.unwrap()).unwrap()["type"], "session_renamed");
    assert_eq!(serde_json::from_str::<Value>(&sidebar.recv().await.unwrap()).unwrap()["type"], "unread_changed");
  }

  async fn test_route(
    State(features): State<Arc<FeatureContext>>,
    Path(session_id): Path<String>,
    RawQuery(query): RawQuery,
    upgrade: WebSocketUpgrade,
  ) -> Response {
    session_websocket(
      features,
      WebSocketRequest {
        path: format!("/ws/sessions/{session_id}"),
        segments: vec![session_id],
        query: query.unwrap_or_default(),
        upgrade,
      },
    )
    .await
  }

  async fn start_server(features: Arc<FeatureContext>) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new().route("/ws/sessions/{session_id}", get(test_route)).with_state(features);
    let server = tokio::spawn(async move {
      axum::serve(listener, app).await.unwrap();
    });
    (address, server)
  }

  async fn connect(address: SocketAddr, session_id: &str, query: &str) -> BufReader<TcpStream> {
    let stream = TcpStream::connect(address).await.unwrap();
    let mut reader = BufReader::new(stream);
    let target = if query.is_empty() {
      format!("/ws/sessions/{session_id}")
    } else {
      format!("/ws/sessions/{session_id}?{query}")
    };
    let request = format!(
      "GET {target} HTTP/1.1\r\nHost: {address}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
       Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
    );
    reader.get_mut().write_all(request.as_bytes()).await.unwrap();
    let mut status = String::new();
    reader.read_line(&mut status).await.unwrap();
    assert!(status.contains(" 101 "), "unexpected handshake response: {status}");
    loop {
      let mut line = String::new();
      reader.read_line(&mut line).await.unwrap();
      if line == "\r\n" || line.is_empty() {
        break;
      }
    }
    reader
  }

  async fn send_client_frame(reader: &mut BufReader<TcpStream>, opcode: u8, payload: &[u8]) {
    assert!(payload.len() < 126);
    let mask = [5, 11, 17, 23];
    let mut frame = vec![0x80 | opcode, 0x80 | payload.len() as u8];
    frame.extend_from_slice(&mask);
    frame.extend(payload.iter().enumerate().map(|(index, byte)| byte ^ mask[index % mask.len()]));
    reader.get_mut().write_all(&frame).await.unwrap();
  }

  #[derive(Debug)]
  enum ServerFrame {
    Text(String),
    Close(Option<u16>),
  }

  async fn receive_frame(reader: &mut BufReader<TcpStream>) -> std::io::Result<ServerFrame> {
    let mut header = [0; 2];
    reader.read_exact(&mut header).await?;
    let opcode = header[0] & 0x0f;
    assert_eq!(header[1] & 0x80, 0, "server frames are not masked");
    let mut length = usize::from(header[1] & 0x7f);
    if length == 126 {
      length = usize::from(reader.read_u16().await?);
    } else if length == 127 {
      length = usize::try_from(reader.read_u64().await?).expect("frame length fits usize");
    }
    let mut payload = vec![0; length];
    reader.read_exact(&mut payload).await?;
    match opcode {
      1 => Ok(ServerFrame::Text(String::from_utf8(payload).unwrap())),
      8 => {
        let code = (payload.len() >= 2).then(|| u16::from_be_bytes([payload[0], payload[1]]));
        Ok(ServerFrame::Close(code))
      }
      _ => panic!("unexpected server WebSocket opcode {opcode}"),
    }
  }

  async fn receive_json(reader: &mut BufReader<TcpStream>) -> Value {
    match receive_frame(reader).await.unwrap() {
      ServerFrame::Text(text) => serde_json::from_str(&text).unwrap(),
      ServerFrame::Close(code) => panic!("unexpected server close {code:?}"),
    }
  }

  async fn receive_catchup_complete(reader: &mut BufReader<TcpStream>) {
    time::timeout(Duration::from_secs(1), async {
      loop {
        if receive_json(reader).await["type"] == "catchup_complete" {
          return;
        }
      }
    })
    .await
    .expect("catchup completes within one second");
  }

  #[tokio::test]
  async fn keepalive_pings_at_thirty_seconds_and_binary_client_frames_end_the_socket() {
    let (_home, features) = test_context();
    write_session(&features, "synthetic", TaskProfile::Manager, 0);
    let (address, server) = start_server(Arc::clone(&features)).await;
    let mut client = connect(address, "synthetic", "cursor=0").await;
    receive_catchup_complete(&mut client).await;
    time::pause();
    tokio::task::yield_now().await;
    time::advance(KEEPALIVE_TIMEOUT).await;
    tokio::task::yield_now().await;
    assert_eq!(receive_json(&mut client).await["type"], "ping");
    time::resume();

    send_client_frame(&mut client, 2, b"binary").await;
    // Pings queued while the clock was paused can precede the close frame.
    loop {
      let frame = time::timeout(Duration::from_secs(30), receive_frame(&mut client)).await.unwrap();
      match frame {
        // The close frame or the connection close both prove the handler ended.
        Err(_) | Ok(ServerFrame::Close(_)) => break,
        Ok(ServerFrame::Text(text)) => assert_eq!(serde_json::from_str::<Value>(&text).unwrap()["type"], "ping"),
      }
    }
    server.abort();
    let _ = server.await;
  }

  #[tokio::test]
  async fn a_trigger_turn_reaches_the_socket_as_a_committed_message() {
    let (_home, features) = test_context();
    write_session(&features, "synthetic", TaskProfile::Manager, 0);
    let (address, server) = start_server(Arc::clone(&features)).await;
    let mut client = connect(address, "synthetic", "cursor=999999").await;
    receive_catchup_complete(&mut client).await;
    let store = Arc::clone(&features.store);
    let turn = tokio::spawn(async move {
      store
        .persist_and_broadcast(
          "synthetic",
          json!({
            "type": "scheduled_trigger",
            "content": "[Scheduled trigger fired] Wake the synthetic session socket.",
            "actor": "system",
            "timestamp": "t1",
          }),
        )
        .await
        .unwrap();
      store
        .persist_and_broadcast(
          "synthetic",
          json!({
            "type": "assistant",
            "message": {
              "role": "assistant",
              "model": "claude-sonnet-5",
              "content": [{"type": "text", "text": "The synthetic stream trigger was handled."}],
            },
            "timestamp": "t2",
          }),
        )
        .await
        .unwrap();
      store
        .persist_and_broadcast(
          "synthetic",
          json!({"type": "master_done", "exit_code": 0, "still_thinking": false, "thinking_seconds": 0, "timestamp": "t3"}),
        )
        .await
        .unwrap();
    });
    tokio::task::yield_now().await;
    let deadline = time::sleep(Duration::from_secs(5));
    tokio::pin!(deadline);
    loop {
      tokio::select! {
        frame = receive_json(&mut client) => {
          if frame["type"] == "message" && frame["message"]["content"] == "The synthetic stream trigger was handled." {
            turn.abort();
            server.abort();
            let _ = server.await;
            return;
          }
        }
        _ = &mut deadline => panic!("the trigger turn never reached the socket as a committed message"),
      }
    }
  }

  #[tokio::test]
  async fn a_sidebar_frame_reaches_the_socket() {
    let (_home, features) = test_context();
    write_session(&features, "synthetic", TaskProfile::Manager, 0);
    let (address, server) = start_server(Arc::clone(&features)).await;
    let mut client = connect(address, "synthetic", "cursor=999999").await;
    receive_catchup_complete(&mut client).await;
    features
      .store
      .broadcast_sidebar(
        "synthetic",
        "running_changed",
        serde_json::Map::from_iter([("has_running_tasks".to_string(), json!(true))]),
      )
      .await
      .unwrap();
    let frame = time::timeout(Duration::from_secs(1), receive_json(&mut client)).await.unwrap();
    assert_eq!(frame["type"], "running_changed");
    assert_eq!(frame["session_id"], "synthetic");
    server.abort();
    let _ = server.await;
  }

  #[tokio::test]
  async fn shutdown_drops_pending_frames_and_closes_every_session_socket_with_code_1000() {
    let (_home, features) = test_context();
    write_session(&features, "synthetic", TaskProfile::Manager, 0);
    write_session(&features, "synthetic-second", TaskProfile::Manager, 0);
    let (address, server) = start_server(Arc::clone(&features)).await;
    let mut first = connect(address, "synthetic", "cursor=0").await;
    receive_catchup_complete(&mut first).await;
    let mut second = connect(address, "synthetic-second", "cursor=0").await;
    receive_catchup_complete(&mut second).await;
    let channel = streaming::session_channel("synthetic");
    features.store.broadcast(&channel, json!({"type": "stream", "sequence": 1})).await.unwrap();
    assert_eq!(receive_json(&mut first).await["sequence"], 1);
    features.store.broadcast(&channel, json!({"type": "stream", "sequence": 2})).await.unwrap();
    features.store.close_subscribers().await;
    for client in [&mut first, &mut second] {
      match time::timeout(Duration::from_secs(1), receive_frame(client)).await.unwrap().unwrap() {
        ServerFrame::Close(Some(1000)) => {}
        ServerFrame::Close(code) => panic!("shutdown close code was {code:?}"),
        ServerFrame::Text(text) => panic!("shutdown sent unexpected frame {text}"),
      }
    }
    server.abort();
    let _ = server.await;
  }

  /// Port of `tests/test_session_ws_catchup.py` `test_invalid_handshake_cursor_closes_before_subscription`
  /// (main `4521a51b2`): every cursor value that test parametrizes, sent in the handshake query. The close
  /// code 4400 is the first and only frame, so no catch-up replay or subscription frame preceded it.
  #[tokio::test]
  async fn invalid_handshake_cursor_closes_before_subscription() {
    let invalid_queries = ["", "cursor=-1", "cursor=abc", &format!("cursor={}", "9".repeat(20))];
    for query in invalid_queries {
      let (_home, features) = test_context();
      seed_manager_session(&features, "synthetic", 0).await;
      let (address, server) = start_server(Arc::clone(&features)).await;
      let mut client = connect(address, "synthetic", query).await;
      match time::timeout(Duration::from_secs(1), receive_frame(&mut client)).await.unwrap().unwrap() {
        ServerFrame::Close(Some(4400)) => {}
        other => panic!("the {query:?} handshake ended with {other:?} instead of the 4400 close"),
      }
      server.abort();
      let _ = server.await;
    }
  }

  /// Port of `tests/test_session_ws_catchup.py` `test_handshake_cursor_controls_replay_after_delayed_accept`
  /// (main `4521a51b2`): the client sends no frame at all, so the replay the query cursor controls must start
  /// without waiting for one. Cursor 9 lies past the seeded session's three events, so the replay sends
  /// nothing and `catchup_complete` is the first frame.
  #[tokio::test]
  async fn handshake_cursor_controls_replay_after_delayed_accept() {
    let (_home, features) = test_context();
    seed_manager_session(&features, "synthetic", 0).await;
    let (address, server) = start_server(Arc::clone(&features)).await;
    let mut client = connect(address, "synthetic", "cursor=9").await;
    assert_eq!(receive_json(&mut client).await["type"], "catchup_complete");
    // The subscription precedes the replay, so a live event reaches the same socket afterwards.
    features
      .store
      .broadcast(&streaming::session_channel("synthetic"), json!({"type": "stream", "sequence": 1}))
      .await
      .unwrap();
    assert_eq!(receive_json(&mut client).await["sequence"], 1);
    server.abort();
    let _ = server.await;
  }
}
