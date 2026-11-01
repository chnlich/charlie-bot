//! Live user-message delivery for web manager turns (`src/runtime/midturn_delivery.py`).
//!
//! One module owns the serialized stdin writes, the replay acknowledgements and the stdin close of one Run. The
//! backend writes the Run prompt as the first stream-json user line and keeps stdin open. The dispatcher offers each
//! pending web user message while the turn runs. The backend's replay of a delivered line becomes a non-rendered
//! `input_delivered` event, and the first result closes stdin.
//!
//! The module is the delivery state both sides share: the agent runtime writes into it, and the task queue reads it.
//! The stream-json launch arguments that turn the path on are the backend's, behind its lifecycle
//! (`BackendLifecycle::midturn_launch_kwargs`).

use std::collections::{HashMap, HashSet};
use std::sync::{
  Arc, Mutex, OnceLock,
  atomic::{AtomicBool, Ordering},
};

use anyhow::{Context, bail};
use charliebot_core::event_types;
use charliebot_store::metadata::{SessionMetadata, TaskProfile};
use charliebot_store::run_records::RunKind;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::sync::watch;

/// The Claude stream-json field that marks a replayed user message. The backend's `translate_replay` and the task
/// dispatcher's replay-line parser both read it, so they match this name instead of spelling it.
pub const IS_REPLAY_FIELD: &str = "isReplay";

/// Whether the spawned child has exited, read by the mid-turn delivery path before it writes.
pub type ExitedProbe = Arc<dyn Fn() -> bool + Send + Sync>;

/// The stdin writer, the child's exit probe and the close flag of one Run's process.
struct StdinState {
  stdin: Option<Box<dyn tokio::io::AsyncWrite + Send + Unpin>>,
  exited: Option<ExitedProbe>,
  closed: bool,
  write_error: Option<String>,
}

/// The replay evidence one Run accumulates: every input the delivery path attempted and wrote.
#[derive(Default)]
struct Evidence {
  attempted: HashSet<String>,
  written: HashSet<String>,
}

/// The serialized stdin writer and replay evidence for one Run (`_DeliveryState`).
struct DeliveryState {
  writer: tokio::sync::Mutex<StdinState>,
  initial_written: (watch::Sender<bool>, watch::Receiver<bool>),
  evidence: Mutex<Evidence>,
  released: AtomicBool,
}

impl DeliveryState {
  fn new() -> Arc<DeliveryState> {
    Arc::new(Self {
      writer: tokio::sync::Mutex::new(StdinState { stdin: None, exited: None, closed: false, write_error: None }),
      initial_written: watch::channel(false),
      evidence: Mutex::new(Evidence::default()),
      released: AtomicBool::new(false),
    })
  }

  async fn wait_initial_written(&self) {
    let mut receiver = self.initial_written.1.clone();
    while !*receiver.borrow() {
      if receiver.changed().await.is_err() {
        return;
      }
    }
  }
}

fn runs() -> &'static Mutex<HashMap<String, Arc<DeliveryState>>> {
  static RUNS: OnceLock<Mutex<HashMap<String, Arc<DeliveryState>>>> = OnceLock::new();
  RUNS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn state_of(run_id: &str) -> Option<Arc<DeliveryState>> {
  runs().lock().expect("mid-turn delivery registry is not poisoned").get(run_id).cloned()
}

/// True for a manager session that no Slack or Discord thread owns (`reply.is_web_session`).
pub fn is_web_session(meta: &SessionMetadata) -> bool {
  meta.profile == TaskProfile::Manager
    && meta.extra.get("slack_origin").is_none_or(Value::is_null)
    && meta.extra.get("discord_origin").is_none_or(Value::is_null)
}

/// Whether this is a web manager turn eligible for live user input.
pub fn is_gated_turn(meta: &SessionMetadata, turn: bool, run_kind: RunKind) -> bool {
  turn && run_kind == RunKind::ManagerTurn && is_web_session(meta)
}

/// The Run id that enables the stream-json input path, or `None` for the plain-text stdin path.
///
/// `Err` when a gated web manager turn carries no Run id: the initial prompt and every offer write name it.
pub fn launch_kwargs(
  meta: &SessionMetadata,
  turn: bool,
  run_kind: RunKind,
  run_id: Option<&str>,
) -> anyhow::Result<Option<String>> {
  if !is_gated_turn(meta, turn, run_kind) {
    return Ok(None);
  }
  let run_id = run_id.filter(|value| !value.is_empty());
  match run_id {
    Some(run_id) => Ok(Some(run_id.to_string())),
    None => bail!("a gated web manager turn needs its Run id"),
  }
}

/// Bind a gated process while preserving this Run's replay evidence.
///
/// `backend_run_id` is `backend.midturn_run_id()`: `None` leaves the plain-text stdin path unbound.
pub async fn bind_process(run_id: &str, backend_run_id: Option<String>) -> anyhow::Result<()> {
  let Some(backend_run_id) = backend_run_id else {
    return Ok(());
  };
  if backend_run_id != run_id {
    bail!("the mid-turn backend Run id does not match its task Run");
  }
  let state = runs()
    .lock()
    .expect("mid-turn delivery registry is not poisoned")
    .entry(run_id.to_string())
    .or_insert_with(DeliveryState::new)
    .clone();
  let mut writer = state.writer.lock().await;
  writer.stdin = None;
  writer.exited = None;
  writer.closed = false;
  writer.write_error = None;
  state.initial_written.0.send_replace(false);
  Ok(())
}

/// Encode one stream-json user input line.
fn user_line(uuid: &str, content: &str) -> Vec<u8> {
  let mut line = json!({
    "type": "user",
    "uuid": uuid,
    "message": {"role": "user", "content": content},
  })
  .to_string()
  .into_bytes();
  line.push(b'\n');
  line
}

/// Write the Run prompt as the first JSON message and keep stdin open.
pub async fn write_initial_prompt(
  run_id: Option<String>,
  stdin: Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
  exited: ExitedProbe,
  prompt: String,
) -> anyhow::Result<()> {
  let run_id = run_id.filter(|value| !value.is_empty()).context("a gated web manager turn needs its Run id")?;
  let state =
    state_of(&run_id).with_context(|| format!("Run {run_id} has no mid-turn delivery state for its initial prompt"))?;
  let mut writer = state.writer.lock().await;
  if *state.initial_written.1.borrow() {
    bail!("Run {run_id} already wrote its initial stream-json prompt");
  }
  writer.exited = Some(exited);
  writer.stdin = Some(stdin);
  let result = write_line(writer.stdin.as_mut().expect("the writer was just stored"), &run_id, &prompt).await;
  if let Err(error) = result {
    writer.write_error = Some(error.to_string());
    state.initial_written.0.send_replace(true);
    return Err(error);
  }
  state.initial_written.0.send_replace(true);
  Ok(())
}

async fn write_line(
  stdin: &mut Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
  uuid: &str,
  content: &str,
) -> anyhow::Result<()> {
  stdin.write_all(&user_line(uuid, content)).await?;
  stdin.flush().await?;
  Ok(())
}

/// Write each composed input once to the live Run's stdin, returning the ids this call wrote.
///
/// `inputs` carries one composed `(event id, content)` pair per pending web user event. An id is attempted once per
/// Run: a later offer of the same event writes nothing.
pub async fn offer(run_id: &str, inputs: Vec<(String, String)>) -> anyhow::Result<Vec<String>> {
  let Some(state) = state_of(run_id) else {
    return Ok(Vec::new());
  };
  state.wait_initial_written().await;
  let mut writer = state.writer.lock().await;
  if let Some(error) = &writer.write_error {
    return Err(anyhow::anyhow!(error.clone()));
  }
  let mut written = Vec::new();
  if state.released.load(Ordering::Acquire) || writer.closed {
    return Ok(written);
  }
  if writer.stdin.is_none() || writer.exited.as_ref().is_some_and(|exited| exited()) {
    return Ok(written);
  }
  for (input_id, content) in inputs {
    {
      let mut evidence = state.evidence.lock().expect("mid-turn evidence mutex is not poisoned");
      if evidence.attempted.contains(&input_id) {
        continue;
      }
      evidence.attempted.insert(input_id.clone());
    }
    write_line(writer.stdin.as_mut().expect("the checked stdin writer exists"), &input_id, &content).await?;
    state.evidence.lock().expect("mid-turn evidence mutex is not poisoned").written.insert(input_id.clone());
    written.push(input_id);
  }
  Ok(written)
}

/// Turn a replay of a delivered user message into a non-rendered acknowledgement.
pub fn translate_replay(run_id: Option<&str>, event: &Value) -> anyhow::Result<Vec<Value>> {
  if event.get("type").and_then(Value::as_str) != Some(event_types::USER)
    || event.get(IS_REPLAY_FIELD) != Some(&Value::Bool(true))
  {
    return Ok(vec![event.clone()]);
  }
  let run_id = run_id.filter(|value| !value.is_empty()).context("a replayed Claude user message has no Run id")?;
  let input_id = match event.get("uuid") {
    Some(Value::String(input_id)) if !input_id.is_empty() => input_id.clone(),
    _ => bail!("a replayed Claude user message has no uuid"),
  };
  if input_id == run_id {
    return Ok(Vec::new());
  }
  if let Some(state) = state_of(run_id)
    && !state.evidence.lock().expect("mid-turn evidence mutex is not poisoned").written.contains(&input_id)
  {
    bail!("Run {run_id} replayed user input {input_id} without a matching stdin write");
  }
  Ok(vec![json!({
    "type": event_types::INPUT_DELIVERED,
    (event_types::INPUT_EVENT_ID): input_id,
    "run_id": run_id,
  })])
}

/// Close gated stdin under the write lock at the first result event.
pub async fn on_backend_event(run_id: Option<String>, event: &Value) -> anyhow::Result<()> {
  let Some(run_id) = run_id else {
    return Ok(());
  };
  if event.get("type").and_then(Value::as_str) != Some(event_types::RESULT) {
    return Ok(());
  }
  let state =
    state_of(&run_id).with_context(|| format!("Run {run_id} has no mid-turn delivery state for its result event"))?;
  state.wait_initial_written().await;
  let mut writer = state.writer.lock().await;
  if writer.closed || state.released.load(Ordering::Acquire) {
    return Ok(());
  }
  if writer.stdin.is_none() {
    bail!("Run {run_id} has no subprocess stdin to close");
  }
  writer.closed = true;
  writer.stdin = None;
  Ok(())
}

/// Release process state after the Run's terminal fact is recorded.
pub fn release_run(run_id: &str) {
  let Some(state) = runs().lock().expect("mid-turn delivery registry is not poisoned").remove(run_id) else {
    return;
  };
  state.released.store(true, Ordering::Release);
  state.initial_written.0.send_replace(true);
}
