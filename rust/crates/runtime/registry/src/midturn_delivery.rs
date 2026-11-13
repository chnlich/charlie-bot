//! Live user-message delivery for web manager turns.
//!
//! One module owns the serialized stdin writes, the replay acknowledgements and the stdin close of one Run. The
//! backend writes the Run prompt as the first stream-json user line and keeps stdin open. Each bound process's
//! first line carries the uuid `initial_line_uuid` derives and `bind_process` registers, so a relayed process never
//! resends a uuid the transcript it resumes already holds. The dispatcher offers each pending web user message
//! while the turn runs. The backend's replay of a delivered line becomes a non-rendered `input_delivered` event,
//! and the first result closes stdin.
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

/// The `backend_kwargs` key that turns the live user-input path on for one Run. The backend's
/// `midturn_launch_kwargs` writes it, and the task dispatcher reads it before binding the process, so both match
/// this name instead of spelling it.
pub const BACKEND_KWARG: &str = "midturn_delivery";

/// The uuid v5 namespace of a relayed process's first-line uuid. Namespaced derivation keeps every relay's uuid out
/// of the random-uuid space that the copied transcript and the pending web inputs draw from.
const FIRST_LINE_NAMESPACE: uuid::Uuid = uuid::Uuid::from_u128(0x5eed_1f0b_4a7d_5c6e_9a2b_3c1d_0e4f_2674);

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
  /// The registered first-line uuid of every bound process, in bind order. The last one names the first line the
  /// current process receives, and its echo is recognized as this Run's own initial line.
  first_lines: Mutex<Vec<String>>,
}

impl DeliveryState {
  fn new() -> Arc<DeliveryState> {
    Arc::new(Self {
      writer: tokio::sync::Mutex::new(StdinState { stdin: None, exited: None, closed: false, write_error: None }),
      initial_written: watch::channel(false),
      evidence: Mutex::new(Evidence::default()),
      released: AtomicBool::new(false),
      first_lines: Mutex::new(Vec::new()),
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

/// True for a manager session that no chat thread owns (`reply.is_web_session`).
pub fn is_web_session(meta: &SessionMetadata) -> bool {
  meta.profile == TaskProfile::Manager && !crate::thread_origin::is_thread_session(meta)
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

/// The binding of a gated process, with this Run's replay evidence preserved.
///
/// `backend_run_id` is `backend.midturn_run_id()`: `None` leaves the plain-text stdin path unbound. Each bind
/// registers the process's first-line uuid, so a relay writes its continuation prompt under a uuid the transcript
/// it resumes does not hold.
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
  let mut first_lines = state.first_lines.lock().expect("first-line mutex is not poisoned");
  let process = first_lines.len();
  first_lines.push(initial_line_uuid(run_id, process));
  drop(first_lines);
  state.initial_written.0.send_replace(false);
  Ok(())
}

/// The stream-json uuid of the first line one gated process of the Run receives. The first process writes the Run
/// id, and every relay derives one from the Run id and its process number. Claude Code 2.1.312 and later skip a
/// stdin line whose uuid the transcript they resume already holds, so a relayed process must never resend the Run
/// id. This function is the one owner of that derivation.
pub fn initial_line_uuid(run_id: &str, process: usize) -> String {
  match process {
    0 => run_id.to_string(),
    _ => uuid::Uuid::new_v5(&FIRST_LINE_NAMESPACE, format!("{run_id}:{process}").as_bytes()).to_string(),
  }
}

/// Whether a replayed uuid names a first line this Run's delivery wrote, rather than a mid-turn web input. Such a
/// uuid is the Run id itself, or the uuid one of the Run's bound processes registered at `bind_process`.
pub fn is_first_line(run_id: &str, uuid: &str) -> bool {
  if uuid == run_id {
    return true;
  }
  state_of(run_id).is_some_and(|state| {
    state.first_lines.lock().expect("first-line mutex is not poisoned").iter().any(|line| line == uuid)
  })
}

/// The encoded stream-json line of one user input.
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

/// The Run prompt written as the first JSON message, with stdin left open.
///
/// The line's uuid is the one `bind_process` registered for this process. It is the Run id for the first process,
/// and a fresh derived uuid for a relay, whose copied transcript already holds the earlier first lines.
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
  let first_line = {
    let first_lines = state.first_lines.lock().expect("first-line mutex is not poisoned");
    first_lines.last().expect("the bound process registered its first-line uuid").clone()
  };
  writer.exited = Some(exited);
  writer.stdin = Some(stdin);
  let result = write_line(writer.stdin.as_mut().expect("the writer was just stored"), &first_line, &prompt).await;
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

/// Each composed input written once to the live Run's stdin, returning the ids this call wrote.
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

/// The non-rendered acknowledgement of a replay of a delivered user message.
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
  if is_first_line(run_id, &input_id) {
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

/// The close of gated stdin under the write lock at the first result event.
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

/// The release of process state after the Run's terminal fact is recorded.
pub fn release_run(run_id: &str) {
  let Some(state) = runs().lock().expect("mid-turn delivery registry is not poisoned").remove(run_id) else {
    return;
  };
  state.released.store(true, Ordering::Release);
  state.initial_written.0.send_replace(true);
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::*;

  fn run_id(suffix: &str) -> String {
    format!("11111111-2222-3333-4444-5555555555{suffix}")
  }

  #[test]
  fn a_relay_uuid_differs_from_every_uuid_the_transcript_holds() {
    let id = run_id("60");
    assert_eq!(initial_line_uuid(&id, 0), id, "the first process keeps the Run id");
    let first_relay = initial_line_uuid(&id, 1);
    let second_relay = initial_line_uuid(&id, 2);
    assert_ne!(first_relay, id);
    assert_ne!(first_relay, second_relay, "every relay of one run derives its own uuid");
    assert!(uuid::Uuid::parse_str(&first_relay).is_ok(), "the derived uuid stays a uuid: {first_relay}");
    let other_run = run_id("61");
    assert_ne!(first_relay, initial_line_uuid(&other_run, 1), "the derivation names the run");
  }

  #[tokio::test]
  async fn the_relay_echo_is_recognized_and_an_unexpected_uuid_stays_an_error() {
    let id = run_id("62");
    let message_id = "22222222-2222-3333-4444-555555555562";
    let relay_uuid = {
      bind_process(&id, Some(id.clone())).await.unwrap();
      bind_process(&id, Some(id.clone())).await.unwrap();
      initial_line_uuid(&id, 1)
    };

    assert!(is_first_line(&id, &id));
    assert!(is_first_line(&id, &relay_uuid));
    assert_eq!(
      translate_replay(Some(&id), &json!({"type": "user", "uuid": relay_uuid, "isReplay": true})).unwrap(),
      Vec::<Value>::new()
    );
    assert!(
      translate_replay(Some(&id), &json!({"type": "user", "uuid": message_id, "isReplay": true})).is_err(),
      "an echo of an unregistered uuid without a matching write stays an error"
    );

    let state = state_of(&id).expect("the bound run has delivery state");
    state.evidence.lock().expect("mid-turn evidence mutex is not poisoned").written.insert(message_id.to_string());
    assert_eq!(
      translate_replay(Some(&id), &json!({"type": "user", "uuid": message_id, "isReplay": true})).unwrap().len(),
      1,
      "the echo of a written web input still becomes one acknowledgement"
    );
    release_run(&id);
  }

  #[tokio::test]
  async fn each_bound_process_writes_its_prompt_under_the_registered_uuid() {
    let id = run_id("63");
    bind_process(&id, Some(id.clone())).await.unwrap();
    let first = written_first_uuid(&id).await;
    assert_eq!(first, id, "the first process writes the Run id");

    bind_process(&id, Some(id.clone())).await.unwrap();
    let relay = written_first_uuid(&id).await;
    assert_eq!(relay, initial_line_uuid(&id, 1));
    assert_ne!(relay, first, "the relayed process resends no uuid the transcript already holds");
    release_run(&id);
  }

  async fn written_first_uuid(run_id: &str) -> String {
    let recorded = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    write_initial_prompt(
      Some(run_id.to_string()),
      Box::new(RecordingStdin(std::sync::Arc::clone(&recorded))),
      Arc::new(|| false),
      "prompt".to_string(),
    )
    .await
    .unwrap();
    let line = String::from_utf8(recorded.lock().expect("recording stdin mutex is not poisoned").clone()).unwrap();
    serde_json::from_str::<Value>(line.trim_end()).expect("the written line is stream-json")["uuid"]
      .as_str()
      .expect("the line carries a uuid")
      .to_string()
  }

  struct RecordingStdin(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

  impl tokio::io::AsyncWrite for RecordingStdin {
    fn poll_write(
      self: std::pin::Pin<&mut Self>,
      _cx: &mut std::task::Context<'_>,
      buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
      self.0.lock().expect("recording stdin mutex is not poisoned").extend_from_slice(buf);
      std::task::Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(
      self: std::pin::Pin<&mut Self>,
      _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
      std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
      self: std::pin::Pin<&mut Self>,
      _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
      std::task::Poll::Ready(Ok(()))
    }
  }
}
