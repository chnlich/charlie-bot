use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use axum::response::Response;
use charliebot_registry::templating::Templating;
use charliebot_registry::{FeatureContext, WebSocketRequest};
use charliebot_store::aggregator::MessageAggregator;
use charliebot_store::metadata::{SessionMetadata, TaskProfile};
use charliebot_store::streaming::{self, APPEND, LENGTH, SocketHandle, Subscription, THINKING_APPEND, THINKING_LENGTH};
use charliebot_store::{RAW_EVENTS_REPLACED_BY_DELTAS, SharedChatEvent, Store, StoreError, StoreResult};
use futures_util::future::BoxFuture;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::time;

use crate::api::CATCHUP_COMPLETE_FRAME;
use crate::route_support::blocking_store;

const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(30);
const CATCHUP_SLICE_EVENTS: usize = 400;

/// The subscription number of the replay a handshake opens: a socket's own first replay is its
/// session's subscription 0, and every client `subscribe` frame takes the page's next number.
const HANDSHAKE_SUBSCRIPTION: u64 = 0;

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

  // The replay cursor rides the handshake query: an absent, overlong or
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

  // The page's static asset version rides the handshake too (`web/static/js/websocket.js`). A tab that
  // stays open across a deploy keeps its old JavaScript, and that JavaScript would keep calling retired
  // routes. A present value that differs from the server's current version closes with 4409 before the
  // subscription starts, so the page reloads to the current code.
  if let Some(page_version) = raw_query_value(&query, "asset_version") {
    match current_asset_version(&features).await {
      Ok(server_version) if server_version == page_version => {}
      Ok(server_version) => {
        tracing::warn!(
          session_id,
          page_version = %page_version,
          server_version = %server_version,
          "session_ws_asset_version_stale"
        );
        if let Err(close_error) = close_with_code(&mut sender, 4409, "").await {
          tracing::info!(session_id, reason = %close_error, "session_ws_closed");
        }
        return;
      }
      Err(error) => {
        tracing::error!(session_id, error = %error, "session_ws_asset_version_failed");
        if let Err(close_error) = close_with_code(&mut sender, 1011, "").await {
          tracing::info!(session_id, reason = %close_error, "session_ws_closed");
        }
        return;
      }
    }
  }

  let mut socket = features.store.register_socket();
  let store = Arc::clone(&features.store);
  let mut sidebar_subscription = store.subscribe(streaming::SIDEBAR_CHANNEL);
  let load_store = Arc::clone(&store);
  let load_session_id = session_id.clone();
  let (mut session_subscription, catchup_result) = store
    .subscribe_session_before_load(&session_id, || async move {
      let meta = load_store.get_session(&load_session_id).await?;
      Ok::<_, StoreError>(prepare_catchup(load_store, load_session_id, cursor, meta).await)
    })
    .await;

  // Frames the subscription queues while the replay is built can repeat events the replay already sent. A
  // repeated stream append would concatenate its text, so those frames are dropped. Both sides compare the
  // event's stamped index, its position in the log, which the replay counts from the same snapshot.
  let mut replayed_through = None;
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
      let complete = catchup_complete_frame(&session_id, HANDSHAKE_SUBSCRIPTION, catchup.total);
      match sender.send(Message::Text(complete.to_string().into())).await {
        Ok(()) => {
          replayed_through = Some(catchup.total);
          tracing::debug!(session_id, cursor, total = catchup.total, sent, "session_ws_catchup_sent");
        }
        Err(error) => tracing::warn!(session_id, error = %error, "session_ws_catchup_failed"),
      }
    }
  }

  let mut state = SessionSocketState { session_id, session_subscription, replayed_through };
  keepalive(&mut sender, &mut receiver, &store, &mut state, &mut sidebar_subscription, &mut socket).await;
  features.store.unsubscribe(&mut state.session_subscription);
  features.store.unsubscribe(&mut sidebar_subscription);
  tracing::info!(session_id = %state.session_id, "session_ws_disconnected");
}

/// The raw value of one key of the handshake query, or `None` when the query has no such key. Repeated
/// keys read the first value, the way Starlette's `query_params.get` does.
fn raw_query_value(query: &str, key: &str) -> Option<String> {
  form_urlencoded::parse(query.as_bytes()).find(|(name, _)| name == key).map(|(_, value)| value.into_owned())
}

/// The raw `cursor` value of the handshake query, or `None` when the query has no `cursor`.
fn raw_cursor(query: &str) -> Option<String> {
  raw_query_value(query, "cursor")
}

/// The server's current static asset version. Reading it walks the served static trees, so it runs on
/// the blocking pool (`Templating::static_asset_version`). A missing `Templating` startup state and a
/// failing walk both come back as the error text.
async fn current_asset_version(features: &FeatureContext) -> Result<String, String> {
  let templating = features.startup.require::<Templating>().map_err(|error| error.to_string())?;
  let version =
    blocking_store(move || templating.static_asset_version().map_err(|error| StoreError::new(error.to_string())))
      .await
      .map_err(|error| error.to_string())?;
  Ok(version)
}

/// The replay cursor the handshake check accepts: the value is present,
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
  // The frames are built from this snapshot, so its end is the replay's end.
  let total = event_index_offset + events.len();
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
  // The replay sends no stream frames. The draft it ends with goes out as one frame, and a page replaces its
  // own draft with it. A page that joins mid-reply therefore renders the text it missed.
  let mut walk = CatchupWalk::new(store.new_message_aggregator(event_index_offset, false), cursor, event_index_offset);
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
}

impl CatchupWalk {
  fn new(aggregator: MessageAggregator, cursor: i128, event_index_offset: usize) -> Self {
    Self { aggregator, cursor, event_index_offset, frames: Vec::new() }
  }

  fn feed_slice<E: CatchupEvent>(&mut self, events: &[E], start: usize, end: usize) -> StoreResult<()> {
    for (index, event) in events.iter().enumerate().take(end).skip(start) {
      let global_index = self.event_index_offset + index;
      let event = event.event();
      let deltas = self.aggregator.feed(&event)?;
      if (global_index as i128) < self.cursor {
        continue;
      }
      self.frames.extend(deltas);
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
    if let Some(draft) = self.aggregator.pending_draft_message() {
      self.frames.push(draft_stream_frame(&draft));
    }
    self.frames
  }
}

/// The whole pending draft as one stream frame that replaces the page's draft: the text, thinking and tool rows
/// a page needs. Those rows let a page render the same draft as a page that received every live frame.
fn draft_stream_frame(draft: &Value) -> Value {
  let mut frame = json!({
    "type": "stream",
    "reset": true,
    "event_index": draft.get("event_index").cloned().unwrap_or(Value::Null),
  });
  let content = draft.get("content").and_then(Value::as_str).unwrap_or_default();
  if !content.is_empty() {
    frame[APPEND] = json!(content);
    frame[LENGTH] = json!(content.encode_utf16().count());
  }
  let thinking = draft.get("thinking").and_then(Value::as_str).unwrap_or_default();
  if !thinking.is_empty() {
    frame[THINKING_APPEND] = json!(thinking);
    frame[THINKING_LENGTH] = json!(thinking.encode_utf16().count());
  }
  if let Some(tools) = draft.get("tools") {
    frame["tools"] = tools.clone();
  }
  frame
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

/// One client `subscribe` frame: the session to serve, the event position the page's snapshot was
/// built from, and the page's subscription number for the request.
struct SubscribeRequest {
  session_id: String,
  cursor: i128,
  subscription: u64,
}

/// One client text frame, as far as the socket's loop cares about it.
enum ClientFrame {
  /// Not a `subscribe` frame: ignored, as every other client frame is.
  Other,
  /// A `subscribe` frame whose fields the server cannot use. `subscription` is the number the frame
  /// named when that field parsed, so the answer can name it for the page to match.
  MalformedSubscribe { subscription: Option<u64> },
  /// A well-formed subscribe request.
  Subscribe(SubscribeRequest),
}

/// Parse one client frame. A frame that does not claim to be a `subscribe` is `Other`; one that
/// claims it and cannot be served is `MalformedSubscribe`.
fn parse_client_frame(text: &str) -> ClientFrame {
  let Ok(frame) = serde_json::from_str::<Value>(text) else {
    return ClientFrame::Other;
  };
  if frame.get("type").and_then(Value::as_str) != Some("subscribe") {
    return ClientFrame::Other;
  }
  let subscription = frame.get("subscription").and_then(Value::as_u64);
  let (Some(session_id), Some(cursor)) =
    (frame.get("session_id").and_then(Value::as_str), frame.get("cursor").and_then(Value::as_u64))
  else {
    return ClientFrame::MalformedSubscribe { subscription };
  };
  let Some(subscription) = subscription else {
    return ClientFrame::MalformedSubscribe { subscription: None };
  };
  ClientFrame::Subscribe(SubscribeRequest { session_id: session_id.to_string(), cursor: cursor as i128, subscription })
}

/// The outcome of one subscribe request's catch-up read.
enum CatchupLoad {
  Loaded(Catchup),
  UnknownSession,
}

/// The `catchup_complete` frame that ends one replay. It names the session and the subscription the replay served,
/// and `event_total` is the count the replay itself ends at.
fn catchup_complete_frame(session_id: &str, subscription: u64, event_total: usize) -> Value {
  json!({
    "type": CATCHUP_COMPLETE_FRAME,
    "session_id": session_id,
    "subscription": subscription,
    "event_total": event_total,
  })
}

/// Handle one `subscribe` frame in the handshake's order: subscribe the new session before the
/// read starts, drop the session the socket leaves, answer `subscribed`. Then replay from the
/// frame's cursor and close with a `catchup_complete` carrying the read's event total. An unknown
/// session or a failed read answers `error` and keeps the previous subscription. Returns `false`
/// when a send failed and the socket must close.
async fn handle_subscribe(
  store: &Arc<Store>,
  sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
  state: &mut SessionSocketState,
  request: SubscribeRequest,
) -> bool {
  let load_store = Arc::clone(store);
  let load_session_id = request.session_id.clone();
  let cursor = request.cursor;
  let (mut new_subscription, loaded) = store
    .subscribe_session_before_load(&request.session_id, || async move {
      match load_store.get_session(&load_session_id).await {
        Err(error) => Err(error),
        Ok(None) => Ok(CatchupLoad::UnknownSession),
        Ok(Some(meta)) => {
          prepare_catchup(load_store, load_session_id, cursor, Some(meta)).await.map(CatchupLoad::Loaded)
        }
      }
    })
    .await;

  let catchup = match loaded {
    Ok(CatchupLoad::Loaded(catchup)) => catchup,
    Ok(CatchupLoad::UnknownSession) => {
      tracing::info!(
        session_id = %request.session_id,
        subscription = request.subscription,
        "session_ws_subscribe_unknown_session"
      );
      store.unsubscribe(&mut new_subscription);
      return send_subscribe_error(sender, Some(request.subscription)).await;
    }
    Err(error) => {
      tracing::warn!(
        session_id = %request.session_id,
        subscription = request.subscription,
        error = %error,
        "session_ws_subscribe_failed"
      );
      store.unsubscribe(&mut new_subscription);
      return send_subscribe_error(sender, Some(request.subscription)).await;
    }
  };

  // The read is done: the socket drops the session it served, binds the target, and answers before
  // the replay. Frames the new subscription queued while the read ran follow the replay, and the
  // dropped subscription discards the frames the left session queued meanwhile.
  store.unsubscribe(&mut state.session_subscription);
  state.session_id = request.session_id.clone();
  state.session_subscription = new_subscription;
  let subscribed = json!({
    "type": "subscribed",
    "session_id": request.session_id,
    "subscription": request.subscription,
  });
  if let Err(error) = sender.send(Message::Text(subscribed.to_string().into())).await {
    tracing::info!(session_id = %request.session_id, reason = %error, "session_ws_closed");
    return false;
  }
  let sent = send_catchup_frames(sender, &catchup.frames, &request.session_id).await;
  // The new replay sets the covered range the loop's own frame arm compares against. The
  // subscription queued the events of the new session while the read ran, and a repeated stream
  // append would concatenate its text a second time.
  state.replayed_through = Some(catchup.total);
  let complete = catchup_complete_frame(&request.session_id, request.subscription, catchup.total);
  if let Err(error) = sender.send(Message::Text(complete.to_string().into())).await {
    tracing::info!(session_id = %request.session_id, reason = %error, "session_ws_closed");
    return false;
  }
  tracing::debug!(
    session_id = %request.session_id,
    cursor,
    total = catchup.total,
    sent,
    subscription = request.subscription,
    "session_ws_subscribe_catchup_sent"
  );
  true
}

/// Answer one refused subscribe request, leaving the socket's subscription untouched. A malformed
/// frame whose subscription number did not parse answers `null` there, because no number it named
/// can be trusted.
async fn send_subscribe_error(
  sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
  subscription: Option<u64>,
) -> bool {
  let error = json!({"type": "error", "subscription": subscription});
  if let Err(send_error) = sender.send(Message::Text(error.to_string().into())).await {
    tracing::info!(reason = %send_error, "session_ws_closed");
    return false;
  }
  true
}

/// The session a socket currently serves: its id, its frame subscription and the event total the
/// last replay ran through, which the frame arm compares live stream frames against.
struct SessionSocketState {
  session_id: String,
  session_subscription: Subscription,
  replayed_through: Option<usize>,
}

async fn keepalive(
  sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
  receiver: &mut futures_util::stream::SplitStream<WebSocket>,
  store: &Arc<Store>,
  state: &mut SessionSocketState,
  sidebar_subscription: &mut Subscription,
  socket: &mut SocketHandle,
) {
  let keepalive = time::sleep(KEEPALIVE_TIMEOUT);
  tokio::pin!(keepalive);
  // A subscribe request waits for the select to return: its handling replaces the session
  // subscription, whose recv future this loop's own arms borrow until the select ends.
  let mut subscribe_request: Option<SubscribeRequest> = None;
  let mut malformed_subscribe: Option<Option<u64>> = None;
  loop {
    tokio::select! {
      incoming = receiver.next() => match incoming {
        Some(Ok(Message::Text(text))) => {
          keepalive.as_mut().reset(time::Instant::now() + KEEPALIVE_TIMEOUT);
          match parse_client_frame(&text) {
            ClientFrame::Other => {}
            ClientFrame::MalformedSubscribe { subscription } => malformed_subscribe = Some(subscription),
            ClientFrame::Subscribe(request) => subscribe_request = Some(request),
          }
        }
        Some(Ok(Message::Close(_))) | None => return,
        Some(Ok(Message::Binary(_))) => {
          tracing::info!(session_id = %state.session_id, reason = "binary frame received", "session_ws_closed");
          return;
        }
        Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
        Some(Err(error)) => {
          tracing::info!(session_id = %state.session_id, reason = %error, "session_ws_closed");
          return;
        }
      },
      frame = state.session_subscription.recv() => match frame {
        Some(frame) => {
          if covered_by_replay(&frame, state.replayed_through) {
            continue;
          }
          if let Err(error) = sender.send(Message::Text(frame.into())).await {
            tracing::info!(session_id = %state.session_id, reason = %error, "session_ws_closed");
            return;
          }
        }
        None => unreachable!("the store keeps a subscriber's sender until this socket unsubscribes"),
      },
      frame = sidebar_subscription.recv() => match frame {
        Some(frame) => if let Err(error) = sender.send(Message::Text(frame.into())).await {
          tracing::info!(session_id = %state.session_id, reason = %error, "session_ws_closed");
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
          tracing::info!(session_id = %state.session_id, reason = %error, "session_ws_closed");
          return;
        }
        keepalive.as_mut().reset(time::Instant::now() + KEEPALIVE_TIMEOUT);
      }
    }
    // A malformed request is refused like an unknown session: an `error` frame that names the
    // request's subscription when it parsed, with the socket's subscription left as it was.
    if let Some(subscription) = malformed_subscribe.take()
      && !send_subscribe_error(sender, subscription).await
    {
      return;
    }
    if let Some(request) = subscribe_request.take()
      && !handle_subscribe(store, sender, state, request).await
    {
      return;
    }
  }
}

/// Whether a queued session frame repeats an event the catch-up replay already sent. Only stream frames need
/// this: a repeated append would concatenate its text a second time, where a repeated committed message is
/// recognized by its id. `replayed_through` is the replay's event count, in the same stamped index the live
/// frames carry; frames of later events are new.
fn covered_by_replay(frame: &str, replayed_through: Option<usize>) -> bool {
  let Some(replayed_through) = replayed_through else { return false };
  let Ok(value) = serde_json::from_str::<Value>(frame) else { return false };
  if value.get("type").and_then(Value::as_str) != Some("stream") {
    return false;
  }
  value.get("event_index").and_then(Value::as_u64).is_some_and(|index| index < replayed_through as u64)
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
  use charliebot_store::metadata::{HAS_RUNNING_TASKS, SessionMetadata, TaskProfile};
  use charliebot_store::run_records::RunRecord;
  use futures_util::FutureExt;
  use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
  use tokio::net::{TcpListener, TcpStream};
  use tokio::time::{self, Duration};

  use super::*;

  fn test_context() -> (crate::test_support::GatedHome, Arc<FeatureContext>) {
    let permit = crate::test_support::hold_sidebar_gate();
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
    (
      crate::test_support::GatedHome::hold(home, permit),
      Arc::new(FeatureContext { config, credentials, store, registry, startup }),
    )
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
  fn handshake_cursor_query_cases_follow_recorded() {
    // A non-empty run of at most 19 ASCII decimal digits is accepted,
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
    assert_eq!(catchup.frames[1]["event_index"], 8);
    assert_eq!(catchup.frames[1]["reset"], true);
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
    assert_eq!(catchup.frames.last().unwrap()["reset"], true);
    assert_eq!(catchup.frames.last().unwrap()["append"], "worker answer");
    assert_eq!(catchup.frames.last().unwrap()["event_index"], 2);
  }

  #[tokio::test]
  async fn the_pending_draft_goes_out_as_one_whole_draft_reset_frame() {
    let (_home, features) = test_context();
    let events: Vec<SharedChatEvent> = vec![json!({
      "type": "assistant",
      "message": {"content": [
        {"type": "text", "text": "draft"},
        {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "ls"}},
      ]},
    })]
    .into_iter()
    .map(|event| SharedChatEvent::new(charliebot_store::ChatEvent::from_value(&event)))
    .collect();
    let frames = replay_events(&features.store, Arc::new(events), 0, 0).await.unwrap();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0]["type"], "stream");
    assert_eq!(frames[0]["reset"], true);
    assert_eq!(frames[0]["append"], "draft");
    assert_eq!(frames[0]["length"], 5);
    // The reset frame carries the whole list and no tools_from: the page replaces its rows with it.
    let tools = frames[0]["tools"].as_array().expect("the draft's tool rows ride the reset frame");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "Bash");
    assert!(frames[0].get("tools_from").is_none());
  }

  /// What a page would render from these frames: the committed assistant messages, and the draft it holds.
  fn rendered_turns(frames: &[Value]) -> (Vec<String>, String) {
    let mut turns = Vec::new();
    let mut draft = String::new();
    for frame in frames {
      match frame["type"].as_str() {
        Some("stream") => {
          if frame["reset"] == true {
            draft.clear();
          }
          if let Some(append) = frame["append"].as_str() {
            draft.push_str(append);
          }
        }
        Some("message") => {
          let message = &frame["message"];
          if message["role"] != "assistant" {
            continue;
          }
          if message[charliebot_core::event_types::MESSAGE_CONTENT_STREAMED] == true {
            turns.push(draft.clone());
          } else {
            turns.push(message["content"].as_str().unwrap_or_default().to_string());
          }
          draft.clear();
        }
        _ => {}
      }
    }
    (turns, draft)
  }

  #[tokio::test]
  async fn live_appends_and_the_catchup_draft_render_the_same_turns() {
    let (_home, features) = test_context();
    let values: Vec<Value> = vec![
      json!({"type": "user", "content": "question"}),
      json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "first "}]}}),
      json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "second"}]}}),
    ];
    let mut live = charliebot_store::aggregator::MessageAggregator::new(0, true);
    live.set_omit_streamed_content(true);
    let mut live_frames = Vec::new();
    for event in &values {
      live_frames.extend(live.feed(event).unwrap());
    }
    let events: Vec<SharedChatEvent> =
      values.iter().map(|event| SharedChatEvent::new(charliebot_store::ChatEvent::from_value(event))).collect();
    let catchup_frames = replay_events(&features.store, Arc::new(events), 0, 0).await.unwrap();

    assert_eq!(rendered_turns(&live_frames), (vec!["first ".to_string()], "second".to_string()));
    assert_eq!(rendered_turns(&live_frames), rendered_turns(&catchup_frames));
  }

  #[test]
  fn a_queued_frame_is_dropped_when_the_replay_already_covered_its_event() {
    let stream = json!({"type": "stream", "append": "x", "length": 1, "event_index": 3}).to_string();
    assert!(covered_by_replay(&stream, Some(4)));
    assert!(!covered_by_replay(&stream, Some(3)), "an event past the replay's end is new");
    assert!(!covered_by_replay(&stream, None), "a failed catch-up covers nothing");
    let message = json!({"type": "message", "message": {"role": "user"}, "event_index": 1}).to_string();
    assert!(!covered_by_replay(&message, Some(4)), "a repeated message is recognized by its id");
  }

  #[tokio::test]
  async fn subscriptions_exist_before_the_catchup_read_starts() {
    let (_home, features) = test_context();
    write_session(&features, "synthetic", TaskProfile::Manager, 0);
    let store = Arc::clone(&features.store);
    let mut sidebar = store.subscribe(streaming::SIDEBAR_CHANNEL);
    let (mut session, events) = store
      .subscribe_session_before_load("synthetic", || async {
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
    let mut streamed = String::new();
    loop {
      tokio::select! {
        frame = receive_json(&mut client) => {
          if frame["type"] == "stream"
            && let Some(append) = frame["append"].as_str()
          {
            streamed.push_str(append);
          }
          if frame["type"] == "message"
            && frame["message"][charliebot_core::event_types::MESSAGE_CONTENT_STREAMED] == true
          {
            // The commit names the draft the page streamed and carries no second copy of its text.
            assert_eq!(streamed, "The synthetic stream trigger was handled.");
            assert!(frame["message"].get("content").is_none());
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
        serde_json::Map::from_iter([(HAS_RUNNING_TASKS.to_string(), json!(true))]),
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

  /// Every cursor value that the handshake check rejects, sent in the handshake query. The close
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

  /// The client sends no frame at all, so the replay the query cursor controls must start without waiting for one.
  /// Cursor 9 lies past the seeded session's three events, so the replay sends
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

  /// The handshake's `catchup_complete` names the session it replayed, the handshake's own
  /// subscription 0 and the read's event total.
  #[tokio::test]
  async fn the_handshake_catchup_complete_carries_the_session_subscription_and_event_total() {
    let (_home, features) = test_context();
    seed_manager_session(&features, "synthetic", 0).await;
    let (address, server) = start_server(Arc::clone(&features)).await;
    let mut client = connect(address, "synthetic", "cursor=999999").await;
    let complete = receive_json(&mut client).await;
    assert_eq!(complete["type"], "catchup_complete");
    assert_eq!(complete["session_id"], "synthetic");
    assert_eq!(complete["subscription"], 0);
    assert_eq!(complete["event_total"], 3);
    server.abort();
    let _ = server.await;
  }

  /// A subscribe frame answers `subscribed` before the replay and closes with a fielded
  /// `catchup_complete`. The socket then serves the target: a live event reaches it once, the
  /// session it left reaches it no more.
  #[tokio::test]
  async fn a_subscribe_answers_subscribed_before_the_replay_and_completes_with_its_fields() {
    let (_home, features) = test_context();
    seed_manager_session(&features, "left", 0).await;
    seed_manager_session(&features, "target", 0).await;
    let (address, server) = start_server(Arc::clone(&features)).await;
    let mut client = connect(address, "left", "cursor=999999").await;
    receive_catchup_complete(&mut client).await;

    send_client_frame(&mut client, 1, br#"{"type":"subscribe","session_id":"target","cursor":1,"subscription":7}"#)
      .await;
    let subscribed = receive_json(&mut client).await;
    assert_eq!(subscribed["type"], "subscribed", "the answer precedes the replay");
    assert_eq!(subscribed["session_id"], "target");
    assert_eq!(subscribed["subscription"], 7);
    let first = receive_json(&mut client).await;
    assert_eq!(first["type"], "task_created");
    assert_eq!(first["event_index"], 2);
    let second = receive_json(&mut client).await;
    assert_eq!(second["type"], "stream");
    let complete = receive_json(&mut client).await;
    assert_eq!(complete["type"], "catchup_complete");
    assert_eq!(complete["session_id"], "target");
    assert_eq!(complete["subscription"], 7);
    assert_eq!(complete["event_total"], 3);

    // The socket serves the target: its live events arrive, the left session's never do.
    features.store.persist_and_broadcast("target", json!({"type": "task_created", "content": "live"})).await.unwrap();
    let live = receive_json(&mut client).await;
    assert_eq!(live["content"], "live");
    features
      .store
      .broadcast(&streaming::session_channel("left"), json!({"type": "stream", "sequence": 1}))
      .await
      .unwrap();
    features.store.persist_and_broadcast("target", json!({"type": "task_created", "content": "second"})).await.unwrap();
    let next = time::timeout(Duration::from_secs(1), receive_json(&mut client)).await.unwrap();
    assert_eq!(next["content"], "second", "the left session's late frame never reaches the socket");
    server.abort();
    let _ = server.await;
  }

  /// An unknown session's subscribe answers `error` with the request's subscription number and
  /// leaves the previous subscription serving the socket.
  #[tokio::test]
  async fn an_unknown_sessions_subscribe_answers_error_and_keeps_the_old_subscription() {
    let (_home, features) = test_context();
    write_session(&features, "synthetic", TaskProfile::Manager, 0);
    let (address, server) = start_server(Arc::clone(&features)).await;
    let mut client = connect(address, "synthetic", "cursor=999999").await;
    receive_catchup_complete(&mut client).await;

    send_client_frame(
      &mut client,
      1,
      br#"{"type":"subscribe","session_id":"no-such-session","cursor":0,"subscription":3}"#,
    )
    .await;
    let error = receive_json(&mut client).await;
    assert_eq!(error["type"], "error");
    assert_eq!(error["subscription"], 3);

    features
      .store
      .broadcast(&streaming::session_channel("synthetic"), json!({"type": "stream", "sequence": 1}))
      .await
      .unwrap();
    let frame = time::timeout(Duration::from_secs(1), receive_json(&mut client)).await.unwrap();
    assert_eq!(frame["sequence"], 1, "the handshake subscription still serves the socket");
    server.abort();
    let _ = server.await;
  }

  /// A malformed subscribe frame answers `error` and leaves the previous subscription serving the
  /// socket, like an unknown session. A frame that names no parseable subscription answers `null`.
  #[tokio::test]
  async fn a_malformed_subscribe_answers_error_and_keeps_the_old_subscription() {
    let (_home, features) = test_context();
    write_session(&features, "synthetic", TaskProfile::Manager, 0);
    let (address, server) = start_server(Arc::clone(&features)).await;
    let mut client = connect(address, "synthetic", "cursor=999999").await;
    receive_catchup_complete(&mut client).await;

    send_client_frame(&mut client, 1, br#"{"type":"subscribe","session_id":7,"cursor":0,"subscription":5}"#).await;
    let error = receive_json(&mut client).await;
    assert_eq!(error["type"], "error");
    assert_eq!(error["subscription"], 5, "the answer names the request's subscription");

    send_client_frame(&mut client, 1, br#"{"type":"subscribe","session_id":"synthetic"}"#).await;
    let error = receive_json(&mut client).await;
    assert_eq!(error["type"], "error");
    assert!(error["subscription"].is_null(), "a frame naming no subscription answers null");

    features
      .store
      .broadcast(&streaming::session_channel("synthetic"), json!({"type": "stream", "sequence": 1}))
      .await
      .unwrap();
    let frame = time::timeout(Duration::from_secs(1), receive_json(&mut client)).await.unwrap();
    assert_eq!(frame["sequence"], 1, "the handshake subscription still serves the socket");
    server.abort();
    let _ = server.await;
  }

  /// An event appended before the subscription reaches the replay alone. The event lock orders
  /// the append-broadcast pair against the subscription's creation, so the frame is queued to no
  /// subscriber and delivered once.
  #[tokio::test]
  async fn an_event_appended_before_the_subscription_reaches_the_replay_alone() {
    let (_home, features) = test_context();
    write_session(&features, "synthetic", TaskProfile::Manager, 0);
    features
      .store
      .persist_and_broadcast("synthetic", json!({"type": "task_created", "content": "before"}))
      .await
      .unwrap();
    let store = Arc::clone(&features.store);
    let load_store = Arc::clone(&store);
    let (mut subscription, events) = store
      .subscribe_session_before_load("synthetic", || async { load_store.load_chat_events_sync("synthetic").unwrap() })
      .await;
    assert_eq!(events.len(), 1);
    // The append and its broadcast finished before the subscription existed, so nothing targets it
    // afterwards. One poll reads the queue and proves it empty without spending the time budget.
    assert!(
      subscription.recv().now_or_never().is_none(),
      "the subscription carries no copy of the event the replay delivers"
    );
  }

  /// An event appended while the catch-up read runs reaches the subscription alone. The read
  /// holds the session's event lock, so the append-broadcast pair waits for it and its event is
  /// absent from the read's snapshot.
  #[tokio::test]
  async fn an_event_appended_while_the_catchup_read_runs_reaches_the_subscription_alone() {
    let (_home, features) = test_context();
    write_session(&features, "synthetic", TaskProfile::Manager, 0);
    let store = Arc::clone(&features.store);
    let load_store = Arc::clone(&store);
    let (mut subscription, loaded) = store
      .subscribe_session_before_load("synthetic", || async {
        let append_store = Arc::clone(&load_store);
        let events = load_store.load_chat_events_sync("synthetic").unwrap();
        let append = tokio::spawn(async move {
          append_store
            .persist_and_broadcast("synthetic", json!({"type": "task_created", "content": "mid-flight"}))
            .await
            .unwrap();
        });
        (events, append)
      })
      .await;
    let (events, append) = loaded;
    append.await.unwrap();
    assert!(events.is_empty(), "the read's snapshot predates the append it blocked");
    let frame = time::timeout(Duration::from_millis(500), subscription.recv()).await.unwrap().unwrap();
    assert_eq!(serde_json::from_str::<Value>(&frame).unwrap()["content"], "mid-flight");
    // The append finished before the first frame was read, and its broadcast copies the frame into
    // the queue before returning, so a duplicate would already be here. One poll proves there is none.
    assert!(
      subscription.recv().now_or_never().is_none(),
      "the subscription delivers the blocked append once, never twice"
    );
  }

  /// A handshake whose `asset_version` differs from the server's current static asset version closes
  /// with 4409 before the subscription starts, so the close frame is the first and only frame.
  #[tokio::test]
  async fn a_stale_asset_version_closes_with_4409_before_any_frame() {
    let (_home, features) = test_context();
    seed_manager_session(&features, "synthetic", 0).await;
    let repo_root = tempfile::tempdir().expect("temporary repo root");
    features.startup.insert(Templating::new(repo_root.path(), &[])).expect("no other Templating was inserted");
    let (address, server) = start_server(Arc::clone(&features)).await;
    let mut client = connect(address, "synthetic", "cursor=0&asset_version=stale-page-version").await;
    match time::timeout(Duration::from_secs(1), receive_frame(&mut client)).await.unwrap().unwrap() {
      ServerFrame::Close(Some(4409)) => {}
      other => panic!("the stale-asset_version handshake ended with {other:?} instead of the 4409 close"),
    }
    server.abort();
    let _ = server.await;
  }

  /// A matching `asset_version` and an absent one both serve the catch-up as before; the matching value
  /// is the one the same `Templating` renders into the page's `?v=` script URLs.
  #[tokio::test]
  async fn a_matching_or_absent_asset_version_reaches_the_catchup() {
    let (_home, features) = test_context();
    seed_manager_session(&features, "synthetic", 0).await;
    let repo_root = tempfile::tempdir().expect("temporary repo root");
    features.startup.insert(Templating::new(repo_root.path(), &[])).expect("no other Templating was inserted");
    let current = features.startup.require::<Templating>().unwrap().static_asset_version().unwrap();
    let (address, server) = start_server(Arc::clone(&features)).await;
    let mut client = connect(address, "synthetic", &format!("cursor=0&asset_version={current}")).await;
    receive_catchup_complete(&mut client).await;
    let mut client = connect(address, "synthetic", "cursor=9").await;
    assert_eq!(receive_json(&mut client).await["type"], "catchup_complete");
    server.abort();
    let _ = server.await;
  }
}
