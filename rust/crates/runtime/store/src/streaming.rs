use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use charliebot_core::event_types;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::{StoreError, StoreResult};

/// Sidebar channel shared by every connected session view.
pub const SIDEBAR_CHANNEL: &str = "sidebar";

/// The trailing stream-delta window used by the Python server.
pub const STREAM_COALESCE_INTERVAL: Duration = Duration::from_millis(200);

const PREVIEW_HIDING_TYPES: &[&str] = &["message", event_types::ASSISTANT_ERROR, "error"];

#[derive(Default)]
struct StreamingState {
  channels: HashMap<String, ChannelState>,
  sockets: HashMap<u64, mpsc::UnboundedSender<CloseRequest>>,
  next_subscriber_id: u64,
  next_socket_id: u64,
  next_window_id: u64,
}

#[derive(Default)]
struct ChannelState {
  subscribers: HashMap<u64, mpsc::UnboundedSender<String>>,
  stream_window: Option<StreamWindow>,
}

struct StreamWindow {
  id: u64,
  pending: Option<String>,
}

struct StreamingInner {
  state: Mutex<StreamingState>,
}

/// A subscriber to one named frame channel.
pub struct Subscription {
  channel: String,
  id: u64,
  receiver: mpsc::UnboundedReceiver<String>,
  manager: Weak<StreamingInner>,
  active: bool,
}

impl Subscription {
  /// Receive the next serialized JSON frame, or no value after unsubscribe or shutdown.
  pub async fn recv(&mut self) -> Option<String> {
    self.receiver.recv().await
  }

  /// Return the channel name.
  pub fn channel(&self) -> &str {
    &self.channel
  }

  /// Return the subscriber id used by the explicit unsubscribe operation.
  pub fn id(&self) -> u64 {
    self.id
  }
}

impl Drop for Subscription {
  fn drop(&mut self) {
    if self.active {
      remove_subscriber(&self.manager, &self.channel, self.id);
      self.active = false;
    }
  }
}

/// The shutdown's request that one open socket send its close frame. The socket replies with the send's outcome.
pub type CloseRequest = oneshot::Sender<Result<(), String>>;

/// One open WebSocket in the shutdown registry. Dropping it removes the socket from the registry.
pub struct SocketHandle {
  id: u64,
  requests: mpsc::UnboundedReceiver<CloseRequest>,
  manager: Weak<StreamingInner>,
}

impl SocketHandle {
  /// Receive the shutdown's close request for this socket.
  pub async fn recv_close_request(&mut self) -> Option<CloseRequest> {
    self.requests.recv().await
  }
}

impl Drop for SocketHandle {
  fn drop(&mut self) {
    remove_socket(&self.manager, self.id);
  }
}

/// Fan frames out to subscribers, with per-channel stream-delta coalescing.
#[derive(Clone)]
pub struct StreamingManager {
  inner: Arc<StreamingInner>,
}

impl Default for StreamingManager {
  fn default() -> Self {
    Self::new()
  }
}

impl StreamingManager {
  /// Create an empty event bus.
  pub fn new() -> Self {
    Self { inner: Arc::new(StreamingInner { state: Mutex::new(StreamingState::default()) }) }
  }

  /// Subscribe to frames on any channel, including the shared sidebar channel.
  pub fn subscribe(&self, channel: &str) -> Subscription {
    let (sender, receiver) = mpsc::unbounded_channel();
    let id = {
      let mut state = self.inner.state.lock().expect("streaming state mutex is not poisoned");
      let id = state.next_subscriber_id;
      state.next_subscriber_id += 1;
      state.channels.entry(channel.to_string()).or_default().subscribers.insert(id, sender);
      id
    };
    Subscription { channel: channel.to_string(), id, receiver, manager: Arc::downgrade(&self.inner), active: true }
  }

  /// Remove one subscription and discard its channel's pending preview when no subscribers remain.
  pub fn unsubscribe(&self, subscription: &mut Subscription) {
    if subscription.active {
      remove_subscriber(&Arc::downgrade(&self.inner), &subscription.channel, subscription.id);
      subscription.active = false;
    }
  }

  /// Broadcast one JSON frame to every subscriber of the named channel.
  pub async fn broadcast(&self, channel: &str, frame: Value) -> StoreResult<()> {
    self.broadcast_with_serializer(channel, frame, serialize_frame).await
  }

  async fn broadcast_with_serializer(
    &self,
    channel: &str,
    frame: Value,
    serializer: impl Fn(&Value) -> StoreResult<String>,
  ) -> StoreResult<()> {
    let serialized = serializer(&frame)?;
    let event_type = frame.get("type").and_then(Value::as_str);
    let is_stream = event_type == Some("stream");
    let hides_preview = event_type.is_some_and(|event_type| PREVIEW_HIDING_TYPES.contains(&event_type));

    if is_stream {
      let window_id = {
        let mut state = self.inner.state.lock().expect("streaming state mutex is not poisoned");
        let Some(channel_state) = state.channels.get_mut(channel) else { return Ok(()) };
        if channel_state.subscribers.is_empty() {
          return Ok(());
        }
        if let Some(window) = channel_state.stream_window.as_mut() {
          window.pending = Some(serialized);
          return Ok(());
        }
        let window_id = state.next_window_id;
        state.next_window_id += 1;
        let channel_state = state.channels.get_mut(channel).expect("channel was checked above");
        channel_state.stream_window = Some(StreamWindow { id: window_id, pending: None });
        send_to_channel(channel_state, serialized);
        window_id
      };
      let inner = Arc::downgrade(&self.inner);
      let channel = channel.to_string();
      let deadline = tokio::time::Instant::now() + STREAM_COALESCE_INTERVAL;
      tokio::spawn(async move {
        tokio::time::sleep_until(deadline).await;
        end_stream_window(inner, &channel, window_id);
      });
      return Ok(());
    }

    let mut state = self.inner.state.lock().expect("streaming state mutex is not poisoned");
    let Some(channel_state) = state.channels.get_mut(channel) else { return Ok(()) };
    if hides_preview {
      channel_state.stream_window = None;
    }
    send_to_channel(channel_state, serialized);
    if channel_state.subscribers.is_empty() {
      state.channels.remove(channel);
    }
    Ok(())
  }

  /// Register one open socket, so that the shutdown closes it. The socket answers each close request from its handle.
  pub fn register_socket(&self) -> SocketHandle {
    let (sender, requests) = mpsc::unbounded_channel();
    let id = {
      let mut state = self.inner.state.lock().expect("streaming state mutex is not poisoned");
      let id = state.next_socket_id;
      state.next_socket_id += 1;
      state.sockets.insert(id, sender);
      id
    };
    SocketHandle { id, requests, manager: Arc::downgrade(&self.inner) }
  }

  /// Discard every pending stream frame, then close each registered socket with code 1000 and wait for its reply.
  /// A close that fails is logged, and the next socket closes. A discarded frame's flush timer finds no window and
  /// does nothing.
  pub async fn close_all(&self) {
    let sockets: Vec<_> = {
      let mut state = self.inner.state.lock().expect("streaming state mutex is not poisoned");
      for channel in state.channels.values_mut() {
        channel.stream_window = None;
      }
      state.sockets.values().cloned().collect()
    };
    for requests in sockets {
      if let Err(error) = close_socket(&requests).await {
        tracing::debug!(error = %error, "ws_close_on_shutdown_failed");
      }
    }
  }
}

/// Return the session-specific channel name.
pub fn session_channel(session_id: &str) -> String {
  format!("session:{session_id}")
}

/// Serialize one frame as the JSON text delivered to subscribers.
pub fn serialize_frame(frame: &Value) -> StoreResult<String> {
  serde_json::to_string(frame)
    .map_err(|error| StoreError::new(format!("websocket frame serialization failed: {error}")))
}

fn send_to_channel(channel: &mut ChannelState, frame: String) {
  channel.subscribers.retain(|_, sender| sender.send(frame.clone()).is_ok());
}

async fn close_socket(requests: &mpsc::UnboundedSender<CloseRequest>) -> Result<(), String> {
  let (reply, outcome) = oneshot::channel();
  requests.send(reply).map_err(|_| "the socket ended before the shutdown reached it".to_string())?;
  outcome.await.map_err(|_| "the socket ended before it replied".to_string())?
}

fn remove_socket(manager: &Weak<StreamingInner>, id: u64) {
  let Some(manager) = manager.upgrade() else { return };
  manager.state.lock().expect("streaming state mutex is not poisoned").sockets.remove(&id);
}

fn remove_subscriber(manager: &Weak<StreamingInner>, channel: &str, id: u64) {
  let Some(manager) = manager.upgrade() else { return };
  let mut state = manager.state.lock().expect("streaming state mutex is not poisoned");
  let Some(channel_state) = state.channels.get_mut(channel) else { return };
  channel_state.subscribers.remove(&id);
  if channel_state.subscribers.is_empty() {
    channel_state.stream_window = None;
    state.channels.remove(channel);
  }
}

fn end_stream_window(manager: Weak<StreamingInner>, channel: &str, window_id: u64) {
  let Some(manager) = manager.upgrade() else { return };
  let mut state = manager.state.lock().expect("streaming state mutex is not poisoned");
  let Some(channel_state) = state.channels.get_mut(channel) else { return };
  if channel_state.stream_window.as_ref().map(|window| window.id) != Some(window_id) {
    return;
  }
  let pending = channel_state.stream_window.take().and_then(|window| window.pending);
  if let Some(pending) = pending {
    send_to_channel(channel_state, pending);
  }
  if channel_state.subscribers.is_empty() {
    state.channels.remove(channel);
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;
  use std::sync::atomic::{AtomicUsize, Ordering};
  use tokio::time::{Duration, advance};

  #[tokio::test(start_paused = true)]
  async fn stream_frames_keep_the_leading_frame_and_flush_the_latest_at_200_ms() {
    let manager = StreamingManager::new();
    let mut subscription = manager.subscribe("session:synthetic");
    manager.broadcast("session:synthetic", json!({"type": "stream", "sequence": 1})).await.unwrap();
    tokio::task::yield_now().await;
    assert_eq!(subscription.recv().await.unwrap(), r#"{"type":"stream","sequence":1}"#);

    advance(Duration::from_millis(199)).await;
    manager.broadcast("session:synthetic", json!({"type": "stream", "sequence": 2})).await.unwrap();
    assert!(subscription.receiver.try_recv().is_err());
    advance(Duration::from_millis(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(subscription.recv().await.unwrap(), r#"{"type":"stream","sequence":2}"#);

    manager.broadcast("session:synthetic", json!({"type": "stream", "sequence": 3})).await.unwrap();
    tokio::task::yield_now().await;
    assert_eq!(subscription.recv().await.unwrap(), r#"{"type":"stream","sequence":3}"#);
  }

  #[tokio::test(start_paused = true)]
  async fn hiding_frames_drop_the_pending_preview_and_open_a_new_window() {
    let manager = StreamingManager::new();
    let mut subscription = manager.subscribe("sidebar");
    manager.broadcast("sidebar", json!({"type": "stream", "sequence": 1})).await.unwrap();
    tokio::task::yield_now().await;
    assert!(subscription.recv().await.unwrap().contains("\"sequence\":1"));
    manager.broadcast("sidebar", json!({"type": "stream", "sequence": 2})).await.unwrap();
    manager.broadcast("sidebar", json!({"type": "message"})).await.unwrap();
    assert_eq!(subscription.recv().await.unwrap(), r#"{"type":"message"}"#);
    manager.broadcast("sidebar", json!({"type": "stream", "sequence": 3})).await.unwrap();
    tokio::task::yield_now().await;
    assert!(subscription.recv().await.unwrap().contains("\"sequence\":3"));
    advance(Duration::from_millis(200)).await;
    tokio::task::yield_now().await;
    assert!(subscription.receiver.try_recv().is_err());
  }

  #[tokio::test]
  async fn stream_frames_without_subscribers_are_dropped() {
    let manager = StreamingManager::new();
    manager.broadcast("session:empty", json!({"type": "stream"})).await.unwrap();
  }

  #[tokio::test]
  async fn broadcast_fans_one_serialized_frame_to_each_subscriber() {
    let manager = StreamingManager::new();
    let mut first = manager.subscribe("session:synthetic");
    let mut second = manager.subscribe("session:synthetic");
    let serialize_calls = Arc::new(AtomicUsize::new(0));
    let call_count = Arc::clone(&serialize_calls);
    manager
      .broadcast_with_serializer("session:synthetic", json!({"type": "error", "message": "failed"}), |frame| {
        call_count.fetch_add(1, Ordering::SeqCst);
        serialize_frame(frame)
      })
      .await
      .unwrap();
    let first_frame = first.recv().await.unwrap();
    assert_eq!(second.recv().await.unwrap(), first_frame);
    assert_eq!(serialize_calls.load(Ordering::SeqCst), 1);

    manager.unsubscribe(&mut first);
    assert!(first.recv().await.is_none());
    manager.broadcast("session:synthetic", json!({"type": "notice"})).await.unwrap();
    assert_eq!(second.recv().await.unwrap(), r#"{"type":"notice"}"#);
  }

  #[tokio::test]
  async fn close_all_asks_the_next_socket_after_a_failed_close() {
    let manager = StreamingManager::new();
    let mut failing = manager.register_socket();
    let mut closing = manager.register_socket();
    let answer_failing = async { failing.recv_close_request().await.unwrap().send(Err("broken pipe".into())).unwrap() };
    let answer_closing = async { closing.recv_close_request().await.unwrap().send(Ok(())).unwrap() };
    tokio::time::timeout(Duration::from_secs(1), async {
      tokio::join!(manager.close_all(), answer_failing, answer_closing)
    })
    .await
    .expect("the shutdown asks every socket to close");
  }

  #[test]
  fn frame_serialization_keeps_compact_unicode_json() {
    assert_eq!(
      serialize_frame(&json!({"type": "message", "content": "café", "count": 1.0})).unwrap(),
      r#"{"type":"message","content":"café","count":1.0}"#
    );
  }
}
