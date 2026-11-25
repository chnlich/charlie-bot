//! The live stream has one broadcast channel per session view. A trailing window merges a burst of stream frames into
//! one. This module also holds the field names a stream frame carries, and the shaping of one event's output into the
//! frames a page reads.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use charliebot_core::event_types;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::chat_event_type::{ChatEventType, log_unnamed};
use crate::{StoreError, StoreResult};

/// Sidebar channel shared by every connected session view.
pub const SIDEBAR_CHANNEL: &str = "sidebar";

/// The frame field carrying a streamed draft's commit: the committed message's id, timestamp and event index. The page
/// rebuilds the assistant message from it, so the frame does not repeat the message's role or streamed-content flag.
pub const COMMIT: &str = "commit";

/// The trailing stream-delta window.
pub const STREAM_COALESCE_INTERVAL: Duration = Duration::from_millis(200);

/// The stream frame field holding the answer text added since the previous frame. The aggregator writes it, the
/// window merges it, and `session-api` writes it into the catch-up frame a page replays after missing the live frames.
pub const APPEND: &str = "append";

/// The stream frame field holding the answer's UTF-16 length after the frame, so a page can tell a replayed append
/// from new text.
pub const LENGTH: &str = "length";

/// The stream frame field holding the thinking text to append to the page's held thinking. A frame that extends the
/// earlier thinking carries only the new text. A frame that replaces it carries the whole buffer.
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
    // A stream frame that carries a commit merges into no window. The commit must reach the page before the appends of
    // any later frame. A second commit could not share the frame's one commit field.
    let carries_commit = frame.get(COMMIT).is_some();

    if event_type == Some("stream") && !carries_commit {
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
    // A commit hides the page's preview whether it arrives as its own message or inside a stream frame.
    let hides_preview =
      carries_commit || event_type.is_some_and(|event_type| PREVIEW_HIDING_TYPES.contains(&event_type));
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

/// The socket frames of one event, shaped for the page that reads them. `deltas` are the aggregator's frames for the
/// event, and `event` is the event with its absolute index `index`. The live feed and the catch-up replay both send
/// every event through here, so the two paths carry the same fields.
///
/// A `master_done` frame keeps the fields the page reads, and its round separator is not sent: the page draws the
/// separator from the frame's id, timestamp and index. A `result` frame keeps its index. Other raw events go out whole.
/// An empty `uploaded_files` and a null `input_mode` on a user echo are left out, as the page reads them as absent. A
/// streamed-draft commit folds into the frame that follows it in the same event (see [`fold_commits`]).
pub fn socket_frames(event: &Value, index: usize, deltas: Vec<Value>) -> StoreResult<Vec<Value>> {
  let mut frames = deltas;
  for frame in &mut frames {
    trim_user_echo(frame);
  }
  let passes_payload = match ChatEventType::of(event.get("type").and_then(Value::as_str)) {
    ChatEventType::DiscordReply => true,
    ChatEventType::SlackReply => true,
    ChatEventType::WebAskCheck => false,
    ChatEventType::DraftFailed => true,
    ChatEventType::DraftRegistered => true,
    ChatEventType::DraftSent => true,
    ChatEventType::DiscordAck => true,
    ChatEventType::WebStatusConfirmed => false,
    ChatEventType::Assistant => false,
    ChatEventType::System => true,
    ChatEventType::ContextCompacted => true,
    ChatEventType::Result => {
      frames.push(json!({"type": event_types::RESULT, event_types::EVENT_INDEX: index}));
      false
    }
    ChatEventType::AgentMessage => true,
    ChatEventType::AssistantError => true,
    ChatEventType::BackendOverlayInactive => true,
    ChatEventType::BackendOverlayUndeclared => true,
    ChatEventType::BackendSwitched => true,
    ChatEventType::ChildReport => false,
    ChatEventType::CloneStart => true,
    ChatEventType::ContextCompactFailed => true,
    ChatEventType::HandlerResult => true,
    ChatEventType::MasterDone => {
      let position = frames
        .iter()
        .position(is_round_separator)
        .ok_or_else(|| StoreError::new("master_done produced no round separator"))?;
      let separator = frames.remove(position);
      let round = &separator["message"];
      frames.push(json!({
        "type": event_types::MASTER_DONE,
        event_types::STILL_THINKING: event.get(event_types::STILL_THINKING).cloned().unwrap_or(Value::Null),
        "id": round["id"],
        "timestamp": round["timestamp"],
        event_types::EVENT_INDEX: index,
      }));
      false
    }
    ChatEventType::ModelFallbackNotice => true,
    ChatEventType::PromptChanged => true,
    ChatEventType::ResumeContextDropped => true,
    ChatEventType::RunDelivery => true,
    ChatEventType::RunFinished => true,
    ChatEventType::RunHeader => true,
    ChatEventType::RunLaunchWithheld => true,
    ChatEventType::RunStopRequested => true,
    ChatEventType::ScheduledRunSkipped => true,
    ChatEventType::ScheduledTrigger => false,
    ChatEventType::SessionAttached => true,
    ChatEventType::SessionRenamed => true,
    ChatEventType::TaskCloseRequested => true,
    ChatEventType::TaskClosed => false,
    ChatEventType::TaskCreated => true,
    ChatEventType::TaskDelegated => true,
    ChatEventType::TaskImported => true,
    ChatEventType::TaskInputAcknowledged => true,
    ChatEventType::TaskReopened => false,
    ChatEventType::TaskTriggered => true,
    ChatEventType::Thinking => true,
    ChatEventType::ToolResult => true,
    ChatEventType::ToolUse => true,
    ChatEventType::User => false,
    ChatEventType::WebReply => false,
    ChatEventType::WebReplyNotice => false,
    ChatEventType::WebReplyCheck => false,
    ChatEventType::WebStatus => false,
    ChatEventType::TurnHooks => true,
    ChatEventType::ContinuationRan => true,
    ChatEventType::FeishuAck => true,
    ChatEventType::SlackAck => true,
    ChatEventType::InputDelivered => true,
    ChatEventType::ImproveFailed => true,
    ChatEventType::ImproveIterationCompleted => true,
    ChatEventType::Error => true,
    ChatEventType::Message => true,
    ChatEventType::Text => true,
    ChatEventType::QuotaFailover => true,
    ChatEventType::FeishuReply => true,
    ChatEventType::PolicyTableChanged => true,
    ChatEventType::WorkerSummary => true,
    ChatEventType::RunEnd => true,
    ChatEventType::RunError => true,
    ChatEventType::TaskReparented => true,
    ChatEventType::ClaudeAccountLoginRequired => true,
    ChatEventType::CommandLifecycle => true,
    ChatEventType::FeatureEvent => true,
    ChatEventType::RateLimitEvent => true,
    ChatEventType::Stream => true,
    ChatEventType::WebStatusSubtree => true,
    ChatEventType::Untyped => true,
    ChatEventType::Unnamed(name) => {
      log_unnamed("socket_frames", "event type", &name, event.get("id").and_then(Value::as_str));
      true
    }
  };
  if passes_payload {
    let mut payload = event.clone();
    let object = payload.as_object_mut().ok_or_else(|| StoreError::new("chat event must be a JSON object"))?;
    object.entry(event_types::EVENT_INDEX).or_insert_with(|| json!(index));
    frames.push(payload);
  }
  Ok(fold_commits(frames))
}

/// Whether a frame is a round separator: the separator a `master_done` renders, which has no kind. A run's end
/// separator carries its kind and stays a message of its own.
fn is_round_separator(frame: &Value) -> bool {
  frame.get("type").and_then(Value::as_str) == Some("message")
    && frame["message"]["role"] == "separator"
    && frame["message"].get("kind").is_none()
}

/// Leaves out a user echo's empty `uploaded_files` and null `input_mode`.
fn trim_user_echo(frame: &mut Value) {
  if frame.get("type").and_then(Value::as_str) != Some("message") {
    return;
  }
  let Some(message) = frame.get_mut("message").and_then(Value::as_object_mut) else { return };
  if message.get("role").and_then(Value::as_str) != Some("user") {
    return;
  }
  if message.get(event_types::UPLOADED_FILES).and_then(Value::as_array).is_some_and(Vec::is_empty) {
    message.remove(event_types::UPLOADED_FILES);
  }
  if message.get(event_types::INPUT_MODE).is_some_and(Value::is_null) {
    message.remove(event_types::INPUT_MODE);
  }
}

/// Folds each streamed-draft commit into the frame after it in the same event, as the frame's [`COMMIT`] field. A
/// commit with no following frame in its event stays a message frame of its own, and so does a commit followed by
/// another commit.
fn fold_commits(frames: Vec<Value>) -> Vec<Value> {
  let mut shaped = Vec::with_capacity(frames.len());
  let mut held: Option<Value> = None;
  for frame in frames {
    if is_streamed_commit(&frame) {
      if let Some(previous) = held.replace(frame) {
        shaped.push(previous);
      }
      continue;
    }
    match held.take() {
      Some(commit) => shaped.push(with_commit(frame, &commit)),
      None => shaped.push(frame),
    }
  }
  shaped.extend(held);
  shaped
}

/// Whether a frame is a `message` that commits a draft the page streamed. Only the live feed commits such a draft.
fn is_streamed_commit(frame: &Value) -> bool {
  frame.get("type").and_then(Value::as_str) == Some("message")
    && frame["message"][event_types::MESSAGE_CONTENT_STREAMED] == json!(true)
}

/// Moves a streamed commit's fields, without its role and streamed-content flag, into `frame`'s commit field.
fn with_commit(mut frame: Value, commit: &Value) -> Value {
  let mut fields = commit["message"].clone();
  if let Some(object) = fields.as_object_mut() {
    object.remove("role");
    object.remove(event_types::MESSAGE_CONTENT_STREAMED);
  }
  frame[COMMIT] = fields;
  frame
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
/// previous frame, so the appended text of the two frames adds up. Every other field takes the newer value.
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

  fn streamed_commit(id: &str, index: u64) -> Value {
    json!({"type": "message", "message": {
      "role": "assistant", "content_streamed": true, "id": id, "timestamp": "t", "event_index": index,
    }})
  }

  #[test]
  fn master_done_keeps_its_page_fields_and_draws_its_round_from_the_separator() {
    let event = json!({
      "type": "master_done", "still_thinking": false, "run_id": "r", "backend": "b", "total_ms": 5,
      "id": "m", "timestamp": "t", "event_index": 9,
    });
    let deltas = vec![
      streamed_commit("draft", 7),
      json!({"type": "message", "message": {
        "role": "separator", "thinking_seconds": 0, "id": "m", "timestamp": "t", "event_index": 9,
      }}),
    ];
    let frames = socket_frames(&event, 9, deltas).unwrap();
    assert_eq!(
      frames,
      vec![json!({
        "type": "master_done", "still_thinking": false, "id": "m", "timestamp": "t", "event_index": 9,
        "commit": {"id": "draft", "timestamp": "t", "event_index": 7},
      })]
    );
  }

  #[test]
  fn result_keeps_only_its_index() {
    let event = json!({"type": "result", "result": "", "usage": {"input_tokens": 1}, "id": "x", "event_index": 4});
    assert_eq!(socket_frames(&event, 4, Vec::new()).unwrap(), vec![json!({"type": "result", "event_index": 4})]);
  }

  #[test]
  fn a_commit_with_no_following_frame_stays_a_message() {
    let event = json!({"type": "assistant"});
    let commit = streamed_commit("draft", 2);
    assert_eq!(socket_frames(&event, 2, vec![commit.clone()]).unwrap(), vec![commit]);
  }

  #[test]
  fn a_commit_folds_into_the_stream_frame_after_it_and_a_second_commit_stays_single() {
    let event = json!({"type": "assistant"});
    let deltas = vec![
      streamed_commit("first", 1),
      json!({"type": "stream", "event_index": 2, "append": "b"}),
      streamed_commit("second", 2),
    ];
    assert_eq!(
      socket_frames(&event, 2, deltas).unwrap(),
      vec![
        json!({"type": "stream", "event_index": 2, "append": "b",
          "commit": {"id": "first", "timestamp": "t", "event_index": 1}}),
        streamed_commit("second", 2),
      ]
    );
  }

  #[test]
  fn a_user_echo_leaves_out_an_empty_upload_list_and_a_null_input_mode() {
    let event = json!({"type": "user"});
    let echo = |uploaded: Value, mode: Value| {
      json!({"type": "message", "message": {
        "role": "user", "content": "hi", "uploaded_files": uploaded, "input_mode": mode, "event_index": 3,
      }})
    };
    let frames =
      socket_frames(&event, 3, vec![echo(json!([]), Value::Null), echo(json!(["a.png"]), json!("voice"))]).unwrap();
    assert_eq!(
      frames,
      vec![
        json!({"type": "message", "message": {"role": "user", "content": "hi", "event_index": 3}}),
        json!({"type": "message", "message": {
          "role": "user", "content": "hi", "uploaded_files": ["a.png"], "input_mode": "voice", "event_index": 3,
        }}),
      ]
    );
  }

  #[test]
  fn other_raw_events_go_out_whole_with_their_index() {
    let event = json!({"type": "backend_overlay_inactive", "backend": "b"});
    assert_eq!(
      socket_frames(&event, 5, Vec::new()).unwrap(),
      vec![json!({"type": "backend_overlay_inactive", "backend": "b", "event_index": 5})]
    );
  }

  #[tokio::test(start_paused = true)]
  async fn a_stream_frame_with_a_commit_is_sent_at_once_and_closes_the_window() {
    // Two commits in one window would share the frame's one commit field, so each commit goes out on its own.
    let manager = StreamingManager::new();
    let mut subscription = manager.subscribe("session:synthetic");
    manager.broadcast("session:synthetic", json!({"type": "stream", "append": "a"})).await.unwrap();
    tokio::task::yield_now().await;
    assert_eq!(subscription.recv().await.unwrap(), r#"{"type":"stream","append":"a"}"#);
    manager
      .broadcast("session:synthetic", json!({"type": "stream", "append": "b", "commit": {"id": "c1"}}))
      .await
      .unwrap();
    manager
      .broadcast("session:synthetic", json!({"type": "stream", "append": "c", "commit": {"id": "c2"}}))
      .await
      .unwrap();
    assert_eq!(subscription.recv().await.unwrap(), r#"{"type":"stream","append":"b","commit":{"id":"c1"}}"#);
    assert_eq!(subscription.recv().await.unwrap(), r#"{"type":"stream","append":"c","commit":{"id":"c2"}}"#);
    manager.broadcast("session:synthetic", json!({"type": "stream", "append": "d"})).await.unwrap();
    tokio::task::yield_now().await;
    assert_eq!(subscription.recv().await.unwrap(), r#"{"type":"stream","append":"d"}"#);
  }
}
