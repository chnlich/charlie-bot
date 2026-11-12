//! The live stream: one broadcast channel per session view, the trailing window that merges a burst of stream frames
//! into one, and the field names a stream frame carries.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use charliebot_core::event_types;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::{StoreError, StoreResult};

/// Sidebar channel shared by every connected session view.
pub const SIDEBAR_CHANNEL: &str = "sidebar";

/// The trailing stream-delta window.
pub const STREAM_COALESCE_INTERVAL: Duration = Duration::from_millis(200);

/// The stream frame field holding the answer text added since the previous frame. The aggregator writes it, the
/// window merges it, and `session-api` writes it into the catch-up frame a page replays after missing the live frames.
pub const APPEND: &str = "append";

/// The stream frame field holding the answer's UTF-16 length after the frame, so a page can tell a replayed append
/// from new text.
pub const LENGTH: &str = "length";

/// The stream frame field holding the thinking text to append to the page's held thinking. A frame that extends the
/// earlier thinking carries only the new text; a frame that replaces it carries the whole buffer.
pub const THINKING_APPEND: &str = "thinking_append";

/// The stream frame field holding the thinking's UTF-16 length after the frame.
pub const THINKING_LENGTH: &str = "thinking_length";

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
  /// The pending frame of the window, with every append broadcast since the leading edge merged into it.
  pending: Option<Value>,
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
    let event_type = frame.get("type").and_then(Value::as_str);

    if event_type == Some("stream") {
      let window_id = {
        let mut state = self.inner.state.lock().expect("streaming state mutex is not poisoned");
        let Some(channel_state) = state.channels.get_mut(channel) else { return Ok(()) };
        if channel_state.subscribers.is_empty() {
          return Ok(());
        }
        if let Some(window) = channel_state.stream_window.as_mut() {
          window.pending = Some(match window.pending.take() {
            Some(previous) => merge_stream_frames(previous, frame),
            None => frame,
          });
          return Ok(());
        }
        let serialized = serializer(&frame)?;
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

    let serialized = serializer(&frame)?;
    let hides_preview = event_type.is_some_and(|event_type| PREVIEW_HIDING_TYPES.contains(&event_type));
    let mut state = self.inner.state.lock().expect("streaming state mutex is not poisoned");
    let Some(channel_state) = state.channels.get_mut(channel) else { return Ok(()) };
    // The pending appends were broadcast before this frame, so they reach the page first.
    flush_pending_frame(channel_state);
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

/// Send the window's pending frame now. A frame that follows it must reach the page after the appends it was
/// broadcast after, so every non-stream frame flushes the window first.
fn flush_pending_frame(channel: &mut ChannelState) {
  let pending = channel.stream_window.as_mut().and_then(|window| window.pending.take());
  let Some(pending) = pending else { return };
  match serialize_frame(&pending) {
    Ok(text) => send_to_channel(channel, text),
    Err(error) => tracing::warn!(error = %error, "stream_frame_serialize_failed"),
  }
}

/// Merge a stream frame into the window's pending one. A stream frame carries only the text added since the
/// previous frame, so the appended text of the two frames adds up; every other field takes the newer value.
fn merge_stream_frames(mut previous: Value, next: Value) -> Value {
  let Some(next_object) = next.as_object() else { return next };
  let Some(previous_object) = previous.as_object_mut() else { return next };
  for (key, value) in next_object {
    let text_key = matches!(key.as_str(), APPEND | THINKING_APPEND);
    let merged = match (text_key, previous_object.get(key).and_then(Value::as_str), value.as_str()) {
      (true, Some(have), Some(more)) => Some(format!("{have}{more}")),
      _ => None,
    };
    previous_object.insert(key.clone(), merged.map_or_else(|| value.clone(), Value::String));
  }
  previous
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
    match serialize_frame(&pending) {
      Ok(text) => send_to_channel(channel_state, text),
      Err(error) => tracing::warn!(error = %error, "stream_frame_serialize_failed"),
    }
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
  async fn a_hiding_frame_flushes_the_pending_appends_before_it() {
    // A stream frame carries only new text, so a frame that hides the preview must not swallow the appends
    // broadcast before it. The page builds the message it commits from them.
    let manager = StreamingManager::new();
    let mut subscription = manager.subscribe("sidebar");
    manager.broadcast("sidebar", json!({"type": "stream", "sequence": 1})).await.unwrap();
    tokio::task::yield_now().await;
    assert!(subscription.recv().await.unwrap().contains("\"sequence\":1"));
    manager.broadcast("sidebar", json!({"type": "stream", "sequence": 2})).await.unwrap();
    manager.broadcast("sidebar", json!({"type": "message"})).await.unwrap();
    assert_eq!(subscription.recv().await.unwrap(), r#"{"type":"stream","sequence":2}"#);
    assert_eq!(subscription.recv().await.unwrap(), r#"{"type":"message"}"#);
    manager.broadcast("sidebar", json!({"type": "stream", "sequence": 3})).await.unwrap();
    tokio::task::yield_now().await;
    assert!(subscription.recv().await.unwrap().contains("\"sequence\":3"));
    advance(Duration::from_millis(200)).await;
    tokio::task::yield_now().await;
    assert!(subscription.receiver.try_recv().is_err());
  }

  #[tokio::test(start_paused = true)]
  async fn a_window_sends_one_frame_whose_appends_add_up() {
    let manager = StreamingManager::new();
    let mut subscription = manager.subscribe("session:synthetic");
    manager
      .broadcast("session:synthetic", json!({"type": "stream", "append": "a", "length": 1, "event_index": 1}))
      .await
      .unwrap();
    tokio::task::yield_now().await;
    assert_eq!(subscription.recv().await.unwrap(), r#"{"type":"stream","append":"a","length":1,"event_index":1}"#);
    manager
      .broadcast("session:synthetic", json!({"type": "stream", "append": "b", "length": 2, "event_index": 2}))
      .await
      .unwrap();
    manager
      .broadcast(
        "session:synthetic",
        json!({"type": "stream", "append": "c", "length": 4, "event_index": 3, "tools": [{"id": "t"}]}),
      )
      .await
      .unwrap();
    assert!(subscription.receiver.try_recv().is_err());
    advance(Duration::from_millis(200)).await;
    tokio::task::yield_now().await;
    assert_eq!(
      subscription.recv().await.unwrap(),
      r#"{"type":"stream","append":"bc","length":4,"event_index":3,"tools":[{"id":"t"}]}"#
    );
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
