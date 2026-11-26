//! Live user-message delivery for manager turns, the runtime half that every backend shares.
//!
//! A backend that takes live input supplies a `MidturnCodec`, which owns its wire format: it encodes one stdin write
//! per message and reads the agent's output into three neutral signals (inputs delivered, turn ended, usage). This
//! module owns everything around the codec: the serialized stdin writes, the de-duplication of input ids, the request
//! ledger (an append-only input log written before each write), the `input_delivered` events, and the turn end that
//! closes the input. It names no backend, model, option or CLI, and no wire field.
//!
//! The backend that has no codec keeps today's behavior: its pending input waits for the next turn.

use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{
  Arc, Mutex, OnceLock,
  atomic::{AtomicBool, Ordering},
};

use anyhow::{Context, bail};
use charliebot_core::event_types;
use charliebot_store::metadata::{SessionMetadata, TaskProfile};
use charliebot_store::run_records::{MIDTURN_INPUT_LOG_NAME, RunKind};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::watch;

use crate::backend::Translate;

/// The `backend_kwargs` key that turns the live user-input path on for one Run. The backend's
/// `midturn_launch_kwargs` writes it, and the task dispatcher reads it before binding the process, so both match this
/// name instead of spelling it.
pub const BACKEND_KWARG: &str = "midturn_delivery";

/// Whether the spawned child has exited, read by the mid-turn delivery path before it writes.
pub type ExitedProbe = Arc<dyn Fn() -> bool + Send + Sync>;

/// One write to the agent's input: the request key that names it in the input log, and the bytes to write.
pub struct EncodedInput {
  pub key: String,
  pub line: Vec<u8>,
}

/// A neutral signal of a codec's reading of the agent's output.
#[derive(Debug, PartialEq, Eq)]
pub enum MidturnSignal {
  /// The agent has received the requests named by these keys, each counted once.
  Delivered(Vec<String>),
  /// The agent's turn has ended, with the status the codec gives it. The runtime closes the input under the write lock,
  /// and only while no written request still awaits its answer.
  TurnEnded { succeeded: bool },
}

/// What a codec makes of one raw output event: the events it translates to, and its neutral signals.
pub struct Observed {
  pub events: Vec<Value>,
  pub signals: Vec<MidturnSignal>,
}

/// One backend's mid-turn wire format. The runtime calls it under its own locks; a codec does no I/O of its own.
pub trait MidturnCodec: Send {
  /// The Run's initial prompt as one stdin write.
  fn encode_prompt(&mut self, prompt: &str) -> anyhow::Result<EncodedInput>;

  /// One pending user input, as one stdin write. The key names the write in the input log.
  /// `None` when the codec takes no more input in this turn: the input waits for the next turn.
  fn encode_input(&mut self, input_id: &str, content: &str) -> anyhow::Result<Option<EncodedInput>>;

  /// One raw output event, read in order.
  fn observe(&mut self, raw: &Value) -> anyhow::Result<Observed>;

  /// Whether the turn may end now: no written request still awaits its answer.
  fn turn_may_end(&self) -> bool;

  /// Rebuilds the codec's state after a restart. `logged_keys` names every write the input log holds, and the codec
  /// reads its own output up to `cursor` (the follower's checkpoint) to learn which of them were answered.
  fn resume(&mut self, logged_keys: &[String], transport_dir: &Path, cursor: u64) -> anyhow::Result<()>;
}

/// One input-log record: a write's key, and the input event ids it carries (none for the initial prompt).
#[derive(Serialize, Deserialize)]
struct LedgerLine {
  key: String,
  inputs: Vec<String>,
}

/// The stdin writer and the exit probe of one process of a Run.
struct Writer {
  stdin: Option<Box<dyn AsyncWrite + Send + Unpin>>,
  exited: Option<ExitedProbe>,
  closed: bool,
  write_error: Option<String>,
}

/// The request ledger of one Run: each written key with its input ids, and every input id already logged.
#[derive(Default)]
struct Ledger {
  inputs_by_key: HashMap<String, Vec<String>>,
  logged_inputs: HashSet<String>,
}

/// The turn-end bookkeeping: a candidate end the codec signaled, and whether the turn has ended.
#[derive(Default)]
struct Turn {
  candidate: Option<bool>,
  status: Option<bool>,
}

/// The delivery state of one Run's process: the codec, the writer, the ledger and the turn end.
pub struct DeliveryState {
  run_id: String,
  writer: tokio::sync::Mutex<Writer>,
  codec: Mutex<Box<dyn MidturnCodec>>,
  ledger: Mutex<Ledger>,
  input_log: OnceLock<PathBuf>,
  turn: Mutex<Turn>,
  initial_written: (watch::Sender<bool>, watch::Receiver<bool>),
  released: AtomicBool,
}

impl DeliveryState {
  /// The state of one process of `run_id`, with its codec. The initial prompt is not yet written.
  pub fn new(run_id: &str, codec: Box<dyn MidturnCodec>) -> Arc<Self> {
    Arc::new(Self {
      run_id: run_id.to_string(),
      writer: tokio::sync::Mutex::new(Writer { stdin: None, exited: None, closed: false, write_error: None }),
      codec: Mutex::new(codec),
      ledger: Mutex::new(Ledger::default()),
      input_log: OnceLock::new(),
      turn: Mutex::new(Turn::default()),
      initial_written: watch::channel(false),
      released: AtomicBool::new(false),
    })
  }

  /// The Run directory that holds the input log. Every write names it, so it is set before the first write.
  pub fn set_run_dir(&self, run_dir: &Path) -> anyhow::Result<()> {
    let path = run_dir.join(MIDTURN_INPUT_LOG_NAME);
    let existing = self.input_log.get_or_init(|| path.clone());
    if *existing != path {
      bail!("Run {} names two input logs: {} and {}", self.run_id, existing.display(), path.display());
    }
    Ok(())
  }

  /// Loads the ledger from the input log and lets the codec rebuild its state from its output. A Run with no input log
  /// yet has an empty ledger.
  pub fn resume(&self, transport_dir: &Path, cursor: u64) -> anyhow::Result<()> {
    let logged = self.read_input_log()?;
    let keys = logged.iter().map(|line| line.key.clone()).collect::<Vec<_>>();
    {
      let mut ledger = self.ledger.lock().expect("mid-turn ledger mutex is not poisoned");
      for line in logged {
        ledger.logged_inputs.extend(line.inputs.iter().cloned());
        ledger.inputs_by_key.insert(line.key, line.inputs);
      }
    }
    self.codec.lock().expect("mid-turn codec mutex is not poisoned").resume(&keys, transport_dir, cursor)
  }

  fn read_input_log(&self) -> anyhow::Result<Vec<LedgerLine>> {
    let Some(path) = self.input_log.get() else {
      return Ok(Vec::new());
    };
    let text = match std::fs::read_to_string(path) {
      Ok(text) => text,
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
      Err(error) => return Err(error).with_context(|| format!("reading the input log {}", path.display())),
    };
    text
      .lines()
      .map(|line| {
        serde_json::from_str::<LedgerLine>(line)
          .with_context(|| format!("the input log {} holds a line that is not a ledger record", path.display()))
      })
      .collect()
  }

  /// Appends one write to the input log, before the write itself.
  fn log_write(&self, key: &str, inputs: Vec<String>) -> anyhow::Result<()> {
    let path = self.input_log.get().with_context(|| format!("Run {} has no input log", self.run_id))?;
    let mut line = serde_json::to_string(&LedgerLine { key: key.to_string(), inputs: inputs.clone() })?;
    line.push('\n');
    let mut file = OpenOptions::new()
      .create(true)
      .append(true)
      .open(path)
      .with_context(|| format!("opening the input log {}", path.display()))?;
    file.write_all(line.as_bytes()).with_context(|| format!("appending to the input log {}", path.display()))?;
    let mut ledger = self.ledger.lock().expect("mid-turn ledger mutex is not poisoned");
    ledger.logged_inputs.extend(inputs.iter().cloned());
    ledger.inputs_by_key.insert(key.to_string(), inputs);
    Ok(())
  }

  async fn wait_initial_written(&self) {
    let mut receiver = self.initial_written.1.clone();
    while !*receiver.borrow() {
      if receiver.changed().await.is_err() {
        return;
      }
    }
  }

  /// Reattaches a reopened live input (such as a named pipe) after a server restart.
  pub async fn reattach_stdin(
    &self,
    stdin: Box<dyn AsyncWrite + Send + Unpin>,
    exited: ExitedProbe,
  ) -> anyhow::Result<()> {
    let mut writer = self.writer.lock().await;
    if self.released.load(Ordering::Acquire) || writer.closed {
      return Ok(());
    }
    writer.exited = Some(exited);
    writer.stdin = Some(stdin);
    self.initial_written.0.send_replace(true);
    Ok(())
  }

  /// Writes the Run's initial prompt with its stdin left open. `exited` reports the child's exit.
  pub async fn write_prompt(
    &self,
    stdin: Box<dyn AsyncWrite + Send + Unpin>,
    exited: ExitedProbe,
    prompt: &str,
  ) -> anyhow::Result<()> {
    let mut writer = self.writer.lock().await;
    if *self.initial_written.1.borrow() {
      bail!("Run {} already wrote its initial prompt", self.run_id);
    }
    writer.exited = Some(exited);
    writer.stdin = Some(stdin);
    let result = self.write_prompt_line(&mut writer, prompt).await;
    if let Err(error) = &result {
      writer.write_error = Some(error.to_string());
    }
    self.initial_written.0.send_replace(true);
    result
  }

  async fn write_prompt_line(&self, writer: &mut Writer, prompt: &str) -> anyhow::Result<()> {
    let encoded = self.codec.lock().expect("mid-turn codec mutex is not poisoned").encode_prompt(prompt)?;
    self.log_write(&encoded.key, Vec::new())?;
    send(writer, &encoded.line).await
  }

  /// Each composed input written once to the live stdin, returning the input ids this call wrote.
  ///
  /// `inputs` carries one composed `(input id, content)` pair per pending user event. An id is logged once per Run: a
  /// later offer of the same id writes nothing.
  pub async fn offer(&self, inputs: Vec<(String, String)>) -> anyhow::Result<Vec<String>> {
    self.wait_initial_written().await;
    let mut writer = self.writer.lock().await;
    if let Some(error) = &writer.write_error {
      return Err(anyhow::anyhow!(error.clone()));
    }
    let mut written = Vec::new();
    if self.released.load(Ordering::Acquire) || writer.closed {
      return Ok(written);
    }
    if writer.stdin.is_none() || writer.exited.as_ref().is_some_and(|exited| exited()) {
      return Ok(written);
    }
    for (input_id, content) in inputs {
      if self.ledger.lock().expect("mid-turn ledger mutex is not poisoned").logged_inputs.contains(&input_id) {
        continue;
      }
      let encoded =
        self.codec.lock().expect("mid-turn codec mutex is not poisoned").encode_input(&input_id, &content)?;
      let Some(encoded) = encoded else {
        // The codec takes no more input in this turn, so this input and the rest wait for the next turn.
        break;
      };
      self.log_write(&encoded.key, vec![input_id.clone()])?;
      send(&mut writer, &encoded.line).await?;
      written.push(input_id);
    }
    Ok(written)
  }

  /// Reads one raw output event through the codec. Each delivered request becomes an `input_delivered` event for each
  /// input id it carries, and a turn end is kept as a candidate until `settle_turn` applies it.
  pub fn observe(&self, raw: &Value) -> anyhow::Result<Vec<Value>> {
    let observed = self.codec.lock().expect("mid-turn codec mutex is not poisoned").observe(raw)?;
    let mut events = observed.events;
    for signal in observed.signals {
      match signal {
        MidturnSignal::Delivered(keys) => {
          let ledger = self.ledger.lock().expect("mid-turn ledger mutex is not poisoned");
          for key in keys {
            for input_id in ledger.inputs_by_key.get(&key).into_iter().flatten() {
              events.push(json!({
                "type": event_types::INPUT_DELIVERED,
                (event_types::INPUT_EVENT_ID): input_id,
                "run_id": self.run_id,
              }));
            }
          }
        }
        MidturnSignal::TurnEnded { succeeded } => {
          self.turn.lock().expect("mid-turn turn mutex is not poisoned").candidate = Some(succeeded);
        }
      }
    }
    Ok(events)
  }

  /// Applies a candidate turn end under the write lock. The input closes only when no written request still awaits its
  /// answer; otherwise the candidate lapses, and the answer that clears the last request signals it again.
  ///
  /// When a resumed or drained Run holds no open stdin handle, settling still records the closed state and status so
  /// post-restart follows and scans observe the turn end.
  pub async fn settle_turn(&self) -> anyhow::Result<()> {
    let Some(succeeded) = self.turn.lock().expect("mid-turn turn mutex is not poisoned").candidate.take() else {
      return Ok(());
    };
    if self.writer.lock().await.stdin.is_some() {
      self.wait_initial_written().await;
    }
    let mut writer = self.writer.lock().await;
    if writer.closed || self.released.load(Ordering::Acquire) {
      return Ok(());
    }
    if !self.codec.lock().expect("mid-turn codec mutex is not poisoned").turn_may_end() {
      return Ok(());
    }
    writer.closed = true;
    writer.stdin = None;
    self.turn.lock().expect("mid-turn turn mutex is not poisoned").status = Some(succeeded);
    Ok(())
  }

  /// The status the codec gave the turn's end, once the runtime closed the input for it: `Some(true)` for a turn that
  /// succeeded, `None` while the turn has not ended.
  pub fn turn_status(&self) -> Option<bool> {
    self.turn.lock().expect("mid-turn turn mutex is not poisoned").status
  }

  /// Marks the Run's state released once its terminal fact is recorded.
  fn release(&self) {
    self.released.store(true, Ordering::Release);
    self.initial_written.0.send_replace(true);
  }
}

async fn send(writer: &mut Writer, line: &[u8]) -> anyhow::Result<()> {
  let stdin = writer.stdin.as_mut().context("the mid-turn stdin writer is missing")?;
  stdin.write_all(line).await?;
  stdin.flush().await?;
  Ok(())
}

/// The process states of the Runs this server runs, keyed by Run id, with the count of processes each Run has bound.
#[derive(Default)]
struct RunEntry {
  state: Option<Arc<DeliveryState>>,
  processes: usize,
}

fn runs() -> &'static Mutex<HashMap<String, RunEntry>> {
  static RUNS: OnceLock<Mutex<HashMap<String, RunEntry>>> = OnceLock::new();
  RUNS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn state_of(run_id: &str) -> Option<Arc<DeliveryState>> {
  runs().lock().expect("mid-turn delivery registry is not poisoned").get(run_id).and_then(|entry| entry.state.clone())
}

/// How many processes this server has bound to `run_id`. A relayed process reads it to number itself.
pub fn bound_processes(run_id: &str) -> usize {
  runs().lock().expect("mid-turn delivery registry is not poisoned").get(run_id).map_or(0, |entry| entry.processes)
}

/// Registers `state` as the delivery state of its Run's live process, so the dispatcher can offer inputs to it.
pub fn bind_process(state: Arc<DeliveryState>) {
  let mut registry = runs().lock().expect("mid-turn delivery registry is not poisoned");
  let entry = registry.entry(state.run_id.clone()).or_default();
  entry.state = Some(state);
  entry.processes += 1;
}

/// The translator of a run with live input: each raw event goes through the codec (`DeliveryState::observe`).
pub struct MidturnTranslate(pub Arc<DeliveryState>);

impl Translate for MidturnTranslate {
  fn translate(&mut self, raw: Value) -> anyhow::Result<Vec<Value>> {
    self.0.observe(&raw)
  }
}

/// Whether a manager session takes live user input. Its turn is a manager turn of a manager profile.
pub fn is_manager_session(meta: &SessionMetadata) -> bool {
  meta.profile == TaskProfile::Manager
}

/// Whether this is a manager turn eligible for live user input.
pub fn is_gated_turn(meta: &SessionMetadata, turn: bool, run_kind: RunKind) -> bool {
  turn && run_kind == RunKind::ManagerTurn && is_manager_session(meta)
}

/// The Run id that enables the live input path, or `None` for the plain-text stdin path.
///
/// `Err` when a gated web manager turn carries no Run id: the initial prompt and every offer name it.
pub fn launch_kwargs(
  meta: &SessionMetadata,
  turn: bool,
  run_kind: RunKind,
  run_id: Option<&str>,
) -> anyhow::Result<Option<String>> {
  if !is_gated_turn(meta, turn, run_kind) {
    return Ok(None);
  }
  match run_id.filter(|value| !value.is_empty()) {
    Some(run_id) => Ok(Some(run_id.to_string())),
    None => bail!("a gated web manager turn needs its Run id"),
  }
}

/// Offers `inputs` to the live process of `run_id`, returning the input ids it wrote (`DeliveryState::offer`).
pub async fn offer(run_id: &str, inputs: Vec<(String, String)>) -> anyhow::Result<Vec<String>> {
  let Some(state) = state_of(run_id) else {
    return Ok(Vec::new());
  };
  state.offer(inputs).await
}

/// Releases the Run's process state once its terminal fact is recorded.
pub fn release_run(run_id: &str) {
  let Some(entry) = runs().lock().expect("mid-turn delivery registry is not poisoned").remove(run_id) else {
    return;
  };
  if let Some(state) = entry.state {
    state.release();
  }
}

/// Reads the input log of a Run directory: the keys of every write it logged.
pub fn logged_keys(run_dir: &Path) -> anyhow::Result<Vec<String>> {
  let path = run_dir.join(MIDTURN_INPUT_LOG_NAME);
  match std::fs::read_to_string(&path) {
    Ok(text) => text
      .lines()
      .map(|line| {
        serde_json::from_str::<LedgerLine>(line)
          .map(|record| record.key)
          .with_context(|| format!("the input log {} holds a line that is not a ledger record", path.display()))
      })
      .collect(),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
    Err(error) => Err(error).with_context(|| format!("reading the input log {}", path.display())),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A codec that encodes each write as one line, names it by its input id, and signals delivery and turn end from
  /// the raw events it reads: a `delivered` event delivers the key it names, and a `result` event ends the turn.
  struct TestCodec {
    unanswered: HashSet<String>,
  }

  impl MidturnCodec for TestCodec {
    fn encode_prompt(&mut self, prompt: &str) -> anyhow::Result<EncodedInput> {
      self.unanswered.insert("run".to_string());
      Ok(EncodedInput { key: "run".to_string(), line: format!("{prompt}\n").into_bytes() })
    }

    fn encode_input(&mut self, input_id: &str, content: &str) -> anyhow::Result<Option<EncodedInput>> {
      self.unanswered.insert(input_id.to_string());
      Ok(Some(EncodedInput { key: input_id.to_string(), line: format!("{content}\n").into_bytes() }))
    }

    fn observe(&mut self, raw: &Value) -> anyhow::Result<Observed> {
      let mut signals = Vec::new();
      if let Some(key) = raw.get("delivered").and_then(Value::as_str) {
        self.unanswered.remove(key);
        signals.push(MidturnSignal::Delivered(vec![key.to_string()]));
      }
      if raw.get("type").and_then(Value::as_str) == Some("result") {
        self.unanswered.remove("run");
        signals.push(MidturnSignal::TurnEnded { succeeded: true });
      }
      Ok(Observed { events: vec![raw.clone()], signals })
    }

    fn turn_may_end(&self) -> bool {
      self.unanswered.is_empty()
    }

    fn resume(&mut self, logged_keys: &[String], _transport_dir: &Path, _cursor: u64) -> anyhow::Result<()> {
      self.unanswered.extend(logged_keys.iter().cloned());
      Ok(())
    }
  }

  #[derive(Clone, Default)]
  struct Recording(Arc<Mutex<Vec<u8>>>);

  impl AsyncWrite for Recording {
    fn poll_write(
      self: std::pin::Pin<&mut Self>,
      _cx: &mut std::task::Context<'_>,
      buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
      self.0.lock().expect("recording mutex is not poisoned").extend_from_slice(buf);
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

  fn codec() -> Box<dyn MidturnCodec> {
    Box::new(TestCodec { unanswered: HashSet::new() })
  }

  fn lines(recording: &Recording) -> Vec<String> {
    String::from_utf8(recording.0.lock().unwrap().clone()).unwrap().lines().map(str::to_string).collect()
  }

  #[tokio::test]
  async fn each_input_is_logged_before_its_write_and_written_once() {
    let dir = tempfile::tempdir().unwrap();
    let run_id = "run-offer-once";
    let state = DeliveryState::new(run_id, codec());
    state.set_run_dir(dir.path()).unwrap();
    let recording = Recording::default();
    state.write_prompt(Box::new(recording.clone()), Arc::new(|| false), "prompt").await.unwrap();
    bind_process(Arc::clone(&state));

    assert_eq!(offer(run_id, vec![("m1".into(), "first".into())]).await.unwrap(), ["m1"]);
    assert_eq!(offer(run_id, vec![("m1".into(), "first".into())]).await.unwrap(), Vec::<String>::new());
    assert_eq!(lines(&recording), ["prompt", "first"]);
    assert_eq!(logged_keys(dir.path()).unwrap(), ["run", "m1"]);
    release_run(run_id);
  }

  #[tokio::test]
  async fn a_delivered_request_becomes_one_input_delivered_event_and_a_turn_end_waits_for_answers() {
    let dir = tempfile::tempdir().unwrap();
    let run_id = "run-settle";
    let state = DeliveryState::new(run_id, codec());
    state.set_run_dir(dir.path()).unwrap();
    state.write_prompt(Box::new(Recording::default()), Arc::new(|| false), "prompt").await.unwrap();
    bind_process(Arc::clone(&state));
    offer(run_id, vec![("m2".into(), "second".into())]).await.unwrap();

    // The turn-end candidate of the prompt's answer lapses while `m2` awaits its answer.
    state.observe(&json!({"type": "result"})).unwrap();
    state.settle_turn().await.unwrap();
    assert!(state.turn_status().is_none());

    let events = state.observe(&json!({"delivered": "m2"})).unwrap();
    assert_eq!(events[1]["type"], event_types::INPUT_DELIVERED);
    assert_eq!(events[1][event_types::INPUT_EVENT_ID], "m2");
    assert_eq!(events[1]["run_id"], run_id);
    assert_eq!(state.observe(&json!({"delivered": "m2"})).unwrap().len(), 2, "the codec's own delivery repeats");

    state.observe(&json!({"type": "result"})).unwrap();
    state.settle_turn().await.unwrap();
    assert_eq!(state.turn_status(), Some(true));
    assert_eq!(offer(run_id, vec![("m3".into(), "late".into())]).await.unwrap(), Vec::<String>::new());
    release_run(run_id);
  }

  #[tokio::test]
  async fn a_restarted_state_rebuilds_its_ledger_from_the_input_log() {
    let dir = tempfile::tempdir().unwrap();
    let run_id = "run-resume";
    let first = DeliveryState::new(run_id, codec());
    first.set_run_dir(dir.path()).unwrap();
    first.write_prompt(Box::new(Recording::default()), Arc::new(|| false), "prompt").await.unwrap();
    bind_process(Arc::clone(&first));
    offer(run_id, vec![("m4".into(), "kept".into())]).await.unwrap();
    release_run(run_id);

    let restarted = DeliveryState::new(run_id, codec());
    restarted.set_run_dir(dir.path()).unwrap();
    restarted.resume(dir.path(), 0).unwrap();
    let recording = Recording::default();
    restarted.writer.lock().await.stdin = Some(Box::new(recording.clone()));
    restarted.writer.lock().await.exited = Some(Arc::new(|| false));
    restarted.initial_written.0.send_replace(true);
    assert_eq!(restarted.offer(vec![("m4".into(), "kept".into())]).await.unwrap(), Vec::<String>::new());
    assert_eq!(restarted.offer(vec![("m5".into(), "new".into())]).await.unwrap(), ["m5"]);
    assert_eq!(lines(&recording), ["new"], "the logged input is not written again");
    assert_eq!(logged_keys(dir.path()).unwrap(), ["run", "m4", "m5"]);
  }

  #[tokio::test]
  async fn an_input_log_line_that_is_not_a_ledger_record_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(MIDTURN_INPUT_LOG_NAME), "not json\n").unwrap();
    let state = DeliveryState::new("run-broken-log", codec());
    state.set_run_dir(dir.path()).unwrap();
    assert!(state.resume(dir.path(), 0).is_err());
  }
}
