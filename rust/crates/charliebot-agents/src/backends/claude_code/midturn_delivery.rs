//! Live user-message delivery for web manager turns served by Claude Code (`src/runtime/midturn_delivery.py`).
//!
//! One module owns the serialized stdin writes, the replay acknowledgements and the stdin close of one Run: the
//! backend writes the Run prompt as the first stream-json user line and keeps stdin open, the dispatcher offers each
//! pending web user message while the turn runs, Claude's replay of a delivered line becomes a non-rendered
//! `input_delivered` event, and the first result closes stdin.

use std::collections::{HashMap, HashSet};
use std::sync::{
  Arc, Mutex, OnceLock,
  atomic::{AtomicBool, Ordering},
};

use anyhow::{Context, bail};
use charliebot_core::event_types;
use charliebot_registry::backend::{ExitedProbe, LaunchContext, LaunchKind};
use charliebot_store::metadata::{SessionMetadata, TaskProfile};
use charliebot_store::run_records::RunKind;
use serde_json::{Map, Value, json};
use tokio::io::AsyncWriteExt;
use tokio::sync::watch;

/// The Claude backend types whose manager turns can take the stream-json input path.
pub const CLAUDE_BACKEND_TYPES: [&str; 3] = ["cc-claude", "cc-anthropic-compatible", "cc-openai-compatible"];

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

/// Insert the stream-json launch arguments into `backend_kwargs` when `ctx` is a gated web manager turn
/// (`midturn_delivery.launch_kwargs` at the launch-args site).
pub fn insert_launch_kwargs(backend_kwargs: &mut Map<String, Value>, ctx: &LaunchContext) -> anyhow::Result<()> {
  let meta = ctx.session_meta.lock().expect("launch metadata mutex is not poisoned");
  let Some(run_id) = launch_kwargs(&meta, ctx.kind == LaunchKind::Turn, ctx.run_kind, ctx.run_id.as_deref())? else {
    return Ok(());
  };
  backend_kwargs.insert("midturn_delivery".to_string(), Value::Bool(true));
  backend_kwargs.insert("midturn_run_id".to_string(), Value::String(run_id));
  Ok(())
}

/// Bind a gated Claude process while preserving this Run's replay evidence.
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

/// Encode one Claude stream-json user input line.
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
  todo!("the mid-turn initial write lands with the feature");

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
  todo!("the mid-turn stdin offer lands with the feature");

}

/// Turn a Claude replay of a delivered user message into a non-rendered acknowledgement.
pub fn translate_replay(run_id: Option<&str>, event: &Value) -> anyhow::Result<Vec<Value>> {
  todo!("the mid-turn replay translation lands with the feature");

}

/// Close gated stdin under the write lock at the first result event.
pub async fn on_backend_event(run_id: Option<String>, event: &Value) -> anyhow::Result<()> {
  todo!("the mid-turn stdin close lands with the feature");

}

/// Release process state after the Run's terminal fact is recorded.
pub fn release_run(run_id: &str) {
  let Some(state) = runs().lock().expect("mid-turn delivery registry is not poisoned").remove(run_id) else {
    return;
  };
  state.released.store(true, Ordering::Release);
  state.initial_written.0.send_replace(true);
}

/// The gated translator: Claude replays of delivered inputs become `input_delivered` events.
pub struct MidturnTranslate {
  pub(crate) run_id: Option<String>,
}

impl charliebot_registry::backend::Translate for MidturnTranslate {
  fn translate(&mut self, raw: Value) -> anyhow::Result<Vec<Value>> {
    translate_replay(self.run_id.as_deref(), &raw)
  }
}

#[cfg(test)]
mod tests {
  use std::sync::{Arc, Mutex};
  use std::time::Duration;

  use charliebot_registry::Registry;
  use charliebot_registry::backend::{AgentBackend, Env, LaunchArgs};
  use serde_json::{Value, json};

  use crate::agent_environment::AgentBaseEnvironment;
  use crate::backends::claude_code::backend::ClaudeCodeBackend;
  use crate::runner::{AgentRunner, RunRequest};
  use crate::transport::cgroup::SessionCgroupConfig;

  use super::*;

  fn disabled_cgroup() -> SessionCgroupConfig {
    SessionCgroupConfig::disabled()
  }

  fn midturn_launch_kwargs(run_id: &str) -> LaunchArgs {
    LaunchArgs {
      backend_kwargs: serde_json::Map::from_iter([
        ("midturn_delivery".to_string(), Value::Bool(true)),
        ("midturn_run_id".to_string(), Value::String(run_id.to_string())),
      ]),
      ..LaunchArgs::default()
    }
  }

  // Python: tests/test_claude_code_backend.py::test_replay_becomes_non_rendered_input_delivered_event
  #[test]
  fn replay_becomes_non_rendered_input_delivered_event() {
    let run_id = "11111111-2222-3333-4444-555555555560";
    let message_id = "22222222-2222-3333-4444-555555555560";
    assert_eq!(
      translate_replay(Some(run_id), &json!({"type": "user", "uuid": run_id, "isReplay": true})).unwrap(),
      Vec::<Value>::new()
    );

    let delivered =
      translate_replay(Some(run_id), &json!({"type": "user", "uuid": message_id, "isReplay": true})).unwrap();

    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0]["type"], "input_delivered");
    assert_eq!(delivered[0]["input_event_id"], message_id);
    assert_eq!(delivered[0]["run_id"], run_id);
    assert_eq!(charliebot_store::message_utils::events_to_messages(&delivered).unwrap(), Vec::<Value>::new());
  }

  // Python: tests/test_claude_code_backend.py::test_replay_after_result_resets_post_result_timeout
  // A late replay keeps a same-process second turn alive across long silent tool work.
  #[tokio::test]
  async fn replay_after_result_resets_post_result_timeout() {
    let root = tempfile::tempdir().unwrap();
    let run_id = "11111111-2222-3333-4444-555555555561";
    let message_id = "22222222-2222-3333-4444-555555555561";
    let marker = root.path().join("waiting-for-input");
    let stub = root.path().join("claude-stream-json-stub");
    std::fs::write(
      &stub,
      concat!(
        "#!/bin/sh\n",
        "read -r initial\n",
        ": > \"$WAIT_MARKER\"\n",
        "read -r late\n",
        "printf '%s\\n' '{\"type\":\"result\",\"result\":\"first\",\"usage\":{}}'\n",
        "uuid=$(printf '%s' \"$late\" | sed -n 's/.*\"uuid\":\"\\([^\"]*\\)\".*/\\1/p')\n",
        "printf '{\"type\":\"user\",\"isReplay\":true,\"uuid\":\"%s\",\"message\":{\"role\":\"user\"}}\\n' \"$uuid\"\n",
        "sleep 0.35\n",
        "printf '%s\\n' '{\"type\":\"result\",\"result\":\"second\",\"usage\":{}}'\n",
      ),
    )
    .unwrap();
    {
      use std::os::unix::fs::PermissionsExt;
      std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let backend = ClaudeCodeBackend::from_launch(
      None,
      None,
      Some(stub.display().to_string()),
      false,
      None,
      LaunchArgs { log_dir: Some(root.path().join("run")), ..midturn_launch_kwargs(run_id) },
      disabled_cgroup(),
    )
    .unwrap();
    bind_process(run_id, backend.midturn_run_id()).await.unwrap();
    let events = Arc::new(Mutex::new(Vec::<Value>::new()));
    std::fs::create_dir_all(root.path().join("cwd")).unwrap();
    let mut request = RunRequest::new(
      "Initial prompt".to_string(),
      root.path().join("cwd"),
      AgentBaseEnvironment { values: Env::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]) },
      SessionCgroupConfig::disabled(),
      Arc::new(Registry::new()),
    );
    request.post_result_timeout = Duration::from_millis(100);
    request.cleanup_timeout = Duration::from_secs(2);
    request.environment_overlay = Env::from([("WAIT_MARKER".to_string(), marker.display().to_string())]);
    let collected = Arc::clone(&events);
    request.on_event = Arc::new(move |event| {
      let collected = Arc::clone(&collected);
      Box::pin(async move {
        collected.lock().expect("event mutex is not poisoned").push(event);
        Ok(())
      })
    });
    let runner = Arc::new(AgentRunner::new(
      Box::new(backend),
      LaunchArgs { log_dir: Some(root.path().join("run")), ..LaunchArgs::default() },
    ));
    let run_task = tokio::spawn({
      let runner = Arc::clone(&runner);
      async move { runner.run(request).await }
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !marker.exists() {
      assert!(std::time::Instant::now() < deadline, "the fake CLI did not read the initial JSON user message");
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
    offer(run_id, vec![(message_id.to_string(), "Continue the answer.".to_string())]).await.unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(5), run_task).await.unwrap().unwrap().unwrap();
    assert_eq!(outcome.exit_code, Some(0));
    assert!(outcome.hang_diagnostics.is_none());
    let events = events.lock().expect("event mutex is not poisoned");
    let results = events
      .iter()
      .filter(|event| event["type"] == "result")
      .map(|event| event["result"].as_str().unwrap().to_string())
      .collect::<Vec<_>>();
    assert_eq!(results, ["first", "second"]);
    assert!(events.iter().any(|event| event["type"] == "input_delivered" && event["input_event_id"] == message_id));
    release_run(run_id);
  }
}
