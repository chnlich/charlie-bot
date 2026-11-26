//! The Claude Code side of live user-message delivery for manager turns.
//!
//! The runtime owns the delivery state (`charliebot_registry::midturn_delivery`): the serialized stdin writes, the
//! input log and the turn end. This module holds Claude's wire format as that state's codec, `ClaudeMidturnCodec`.
//! Each input is one stream-json user line. The CLI echoes every line it accepts as a replayed user line, and that echo is the
//! delivery signal. The first line of each relayed process carries a uuid the transcript it resumes does not hold.
//! The stream-json launch arguments are returned to the runtime by the lifecycle (`midturn_launch_kwargs`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use charliebot_core::event_types;
use charliebot_registry::backend::{LaunchContext, LaunchKind};
use charliebot_registry::midturn_delivery::{EncodedInput, MidturnCodec, MidturnSignal, Observed, bound_processes};
use charliebot_store::metadata::SessionMetadata;
use charliebot_store::run_records::{RAW_LOG_NAME, RunKind};
use serde::de::{IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value, json};

/// The stream-json field that marks a replayed user message: the CLI's echo of a line it accepted.
pub const IS_REPLAY_FIELD: &str = "isReplay";

/// The uuid v5 namespace of a relayed process's first-line uuid. Namespaced derivation keeps every relay's uuid out of
/// the random-uuid space that the copied transcript and the pending web inputs draw from.
const FIRST_LINE_NAMESPACE: uuid::Uuid = uuid::Uuid::from_u128(0x5eed_1f0b_4a7d_5c6e_9a2b_3c1d_0e4f_2674);

/// The stream-json uuid of the first line one process of the Run receives. The first process writes the Run id, and
/// every relay derives one from the Run id and its process number. Claude Code 2.1.312 and later skip a stdin line
/// whose uuid the transcript they resume already holds, so a relayed process must never resend the Run id.
pub fn initial_line_uuid(run_id: &str, process: usize) -> String {
  match process {
    0 => run_id.to_string(),
    _ => uuid::Uuid::new_v5(&FIRST_LINE_NAMESPACE, format!("{run_id}:{process}").as_bytes()).to_string(),
  }
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

/// The Claude codec of one process of a Run. `process` numbers the process among the Run's relays; the inputs it or an
/// earlier process of the Run wrote are the echoes it accepts.
pub struct ClaudeMidturnCodec {
  run_id: String,
  process: usize,
  written: HashSet<String>,
}

impl ClaudeMidturnCodec {
  /// The codec of the next process of `run_id`: its number is the count of processes this server has bound, and a
  /// restart extends it past the first lines the Run's logs hold (`resume`).
  pub fn new(run_id: &str) -> Self {
    Self { run_id: run_id.to_string(), process: bound_processes(run_id), written: HashSet::new() }
  }

  /// Whether a uuid names a first line this Run wrote: the Run id, or the first line of this process or an earlier one.
  fn is_first_line(&self, uuid: &str) -> bool {
    uuid == self.run_id || (0..=self.process).any(|process| initial_line_uuid(&self.run_id, process) == uuid)
  }
}

impl MidturnCodec for ClaudeMidturnCodec {
  fn encode_prompt(&mut self, prompt: &str) -> anyhow::Result<EncodedInput> {
    let uuid = initial_line_uuid(&self.run_id, self.process);
    Ok(EncodedInput { key: uuid.clone(), line: user_line(&uuid, prompt) })
  }

  fn encode_input(&mut self, input_id: &str, content: &str) -> anyhow::Result<Option<EncodedInput>> {
    self.written.insert(input_id.to_string());
    Ok(Some(EncodedInput { key: input_id.to_string(), line: user_line(input_id, content) }))
  }

  fn observe(&mut self, raw: &Value) -> anyhow::Result<Observed> {
    if raw.get("type").and_then(Value::as_str) == Some(event_types::RESULT) {
      let succeeded = raw.get(event_types::IS_ERROR) != Some(&Value::Bool(true));
      return Ok(Observed { events: vec![raw.clone()], signals: vec![MidturnSignal::TurnEnded { succeeded }] });
    }
    if raw.get("type").and_then(Value::as_str) != Some(event_types::USER)
      || raw.get(IS_REPLAY_FIELD) != Some(&Value::Bool(true))
    {
      return Ok(Observed { events: vec![raw.clone()], signals: Vec::new() });
    }
    let input_id = match raw.get("uuid") {
      Some(Value::String(input_id)) if !input_id.is_empty() => input_id.clone(),
      _ => bail!("a replayed Claude user message has no uuid"),
    };
    if self.is_first_line(&input_id) {
      return Ok(Observed { events: Vec::new(), signals: Vec::new() });
    }
    if !self.written.contains(&input_id) {
      bail!("Run {} replayed user input {input_id} without a matching stdin write", self.run_id);
    }
    Ok(Observed { events: Vec::new(), signals: vec![MidturnSignal::Delivered(vec![input_id])] })
  }

  /// Claude keeps no request waiting for an answer that the input close would cut off: the first result ends its turn.
  fn turn_may_end(&self) -> bool {
    true
  }

  fn resume(&mut self, logged_keys: &[String], transport_dir: &Path, _cursor: u64) -> anyhow::Result<()> {
    self.written.extend(logged_keys.iter().cloned());
    // The first lines the Run's logs hold belong to earlier processes, so this one takes the first line after them.
    let held = replayed_user_uuids(transport_dir)?;
    while held.contains(&initial_line_uuid(&self.run_id, self.process)) {
      self.process += 1;
    }
    Ok(())
  }
}

/// The three fields of a raw transport line that the replay scan reads. The map visitor keeps the last duplicate key
/// and ignores unrelated fields without constructing their values.
struct ReplayLine {
  kind: Option<Value>,
  is_replay: Option<Value>,
  uuid: Option<Value>,
}

impl<'de> Deserialize<'de> for ReplayLine {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    struct ReplayLineVisitor;

    impl<'de> Visitor<'de> for ReplayLineVisitor {
      type Value = ReplayLine;

      fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a JSON object")
      }

      fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
      where
        A: MapAccess<'de>,
      {
        let mut line = ReplayLine { kind: None, is_replay: None, uuid: None };
        while let Some(key) = map.next_key::<String>()? {
          match key.as_str() {
            "type" => line.kind = Some(map.next_value()?),
            IS_REPLAY_FIELD => line.is_replay = Some(map.next_value()?),
            "uuid" => line.uuid = Some(map.next_value()?),
            _ => {
              let _: IgnoredAny = map.next_value()?;
            }
          }
        }
        Ok(line)
      }
    }

    deserializer.deserialize_map(ReplayLineVisitor)
  }
}

/// Read every replayed user uuid from every raw transport leg of one Run directory.
///
/// A replayed user line is the CLI's echo of a stdin line it accepted. The set therefore holds each uuid the Run's
/// conversation received, the first lines included. A missing directory holds no legs. An unreadable directory or leg
/// is an error, because a uuid the set omits could be one the conversation holds. A raw line that is not a JSON object
/// is skipped, and a line without its newline ends the scan of its leg.
pub fn replayed_user_uuids(transport_dir: &Path) -> anyhow::Result<HashSet<String>> {
  use std::io::BufRead;

  let entries = match std::fs::read_dir(transport_dir) {
    Ok(entries) => entries,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(HashSet::new()),
    Err(error) => {
      return Err(error).with_context(|| format!("reading the run transport {}", transport_dir.display()));
    }
  };
  let mut raw_paths = Vec::<PathBuf>::new();
  for entry in entries {
    let path = entry.with_context(|| format!("reading the run transport {}", transport_dir.display()))?.path();
    let is_leg = path.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.starts_with(RAW_LOG_NAME));
    let is_file =
      std::fs::metadata(&path).with_context(|| format!("reading the raw log {}", path.display()))?.is_file();
    if is_leg && is_file {
      raw_paths.push(path);
    }
  }
  raw_paths.sort();
  let mut replayed = HashSet::new();
  let mut line = Vec::new();
  for raw_path in raw_paths {
    let raw_file =
      std::fs::File::open(&raw_path).with_context(|| format!("reading the raw log {}", raw_path.display()))?;
    let mut raw_lines = std::io::BufReader::new(raw_file);
    loop {
      line.clear();
      raw_lines.read_until(b'\n', &mut line).with_context(|| format!("reading the raw log {}", raw_path.display()))?;
      let Some(complete) = line.strip_suffix(b"\n") else {
        break;
      };
      let Ok(event) = serde_json::from_slice::<ReplayLine>(complete) else {
        continue;
      };
      if event.kind.as_ref().and_then(Value::as_str) != Some(event_types::USER)
        || event.is_replay.as_ref().and_then(Value::as_bool) != Some(true)
      {
        continue;
      }
      if let Some(input_id) = event.uuid.as_ref().and_then(Value::as_str)
        && !input_id.is_empty()
      {
        replayed.insert(input_id.to_string());
      }
    }
  }
  Ok(replayed)
}

/// The launch arguments that enable the stream-json input path when `meta` is a gated web manager turn, else empty.
pub fn launch_kwargs(
  meta: &SessionMetadata,
  turn: bool,
  run_kind: RunKind,
  run_id: Option<&str>,
) -> anyhow::Result<Map<String, Value>> {
  let Some(run_id) = charliebot_registry::midturn_delivery::launch_kwargs(meta, turn, run_kind, run_id)? else {
    return Ok(Map::new());
  };
  Ok(Map::from_iter([
    (charliebot_registry::midturn_delivery::BACKEND_KWARG.to_string(), Value::Bool(true)),
    (charliebot_registry::midturn_delivery::RUN_ID_KWARG.to_string(), Value::String(run_id)),
  ]))
}

/// Insert the stream-json launch arguments into `backend_kwargs` when `ctx` is a gated web manager turn.
pub fn insert_launch_kwargs(backend_kwargs: &mut Map<String, Value>, ctx: &LaunchContext) -> anyhow::Result<()> {
  let meta = ctx.session_meta.lock().expect("launch metadata mutex is not poisoned");
  let arguments = launch_kwargs(&meta, ctx.kind == LaunchKind::Turn, ctx.run_kind, ctx.run_id.as_deref())?;
  backend_kwargs.extend(arguments);
  Ok(())
}

#[cfg(test)]
mod tests {
  use std::sync::{Arc, Mutex};
  use std::time::Duration;

  use charliebot_registry::Registry;
  use charliebot_registry::backend::{AgentBackend, Env, LaunchArgs};
  use charliebot_registry::midturn_delivery::{DeliveryState, bind_process, offer, release_run};
  use serde_json::{Value, json};

  use super::{ClaudeMidturnCodec, initial_line_uuid};

  use crate::backend::ClaudeCodeBackend;
  use charliebot_agents::agent_environment::AgentBaseEnvironment;
  use charliebot_agents::runner::{AgentRunner, RunRequest};
  use charliebot_agents::transport::cgroup::SessionCgroupConfig;

  /// The PATH of a run whose `claude` is the stub at `stub`: the stub's directory, then the system directories.
  fn stub_path(stub: &std::path::Path) -> String {
    format!("{}:/usr/bin:/bin", stub.parent().expect("a stub sits in a directory").display())
  }

  fn disabled_cgroup() -> SessionCgroupConfig {
    SessionCgroupConfig::disabled()
  }

  fn midturn_launch_kwargs(run_id: &str) -> LaunchArgs {
    LaunchArgs {
      extra_flags: Vec::new(),
      buffer_limit: None,
      instructions_content: None,
      instructions_file: None,
      resume_session_id: None,
      log_dir: None,
      cgroup_session_id: Some("midturn-session".to_string()),
      remote_launch: None,
      backend_kwargs: serde_json::Map::from_iter([
        ("midturn_delivery".to_string(), Value::Bool(true)),
        ("midturn_run_id".to_string(), Value::String(run_id.to_string())),
        ("run_id".to_string(), Value::String(run_id.to_string())),
      ]),
      manager_turn: false,
    }
  }

  #[tokio::test]
  async fn replay_becomes_non_rendered_input_delivered_event() {
    let run_id = "11111111-2222-3333-4444-555555555560";
    let message_id = "22222222-2222-3333-4444-555555555560";
    let dir = tempfile::tempdir().unwrap();
    let state = DeliveryState::new(run_id, Box::new(ClaudeMidturnCodec::new(run_id)));
    state.set_run_dir(dir.path()).unwrap();
    state.write_prompt(Box::new(tokio::io::sink()), Arc::new(|| false), "Initial prompt").await.unwrap();
    bind_process(Arc::clone(&state));
    offer(run_id, vec![(message_id.to_string(), "Continue.".to_string())]).await.unwrap();
    assert_eq!(state.observe(&json!({"type": "user", "uuid": run_id, "isReplay": true})).unwrap(), Vec::<Value>::new());

    let delivered = state.observe(&json!({"type": "user", "uuid": message_id, "isReplay": true})).unwrap();

    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0]["type"], "input_delivered");
    assert_eq!(delivered[0]["input_event_id"], message_id);
    assert_eq!(delivered[0]["run_id"], run_id);
    let compact = delivered.iter().map(charliebot_store::ChatEvent::from_value).collect::<Vec<_>>();
    assert_eq!(
      charliebot_store::projection::events_to_messages(
        &compact,
        0,
        Default::default(),
        charliebot_core::config::BackendsConfig::default()
      )
      .unwrap(),
      Vec::<Value>::new()
    );
  }

  #[ignore = "integration"]
  // A late replay keeps a same-process second turn alive across long silent tool work.
  #[tokio::test]
  async fn replay_after_result_resets_post_result_timeout() {
    let root = tempfile::tempdir().unwrap();
    let run_id = "11111111-2222-3333-4444-555555555561";
    let message_id = "22222222-2222-3333-4444-555555555561";
    let marker = root.path().join("waiting-for-input");
    let bin = root.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let stub = bin.join("claude");
    charliebot_shell_harness::write_executable(
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
    );
    let backend = ClaudeCodeBackend::from_launch(
      None,
      None,
      false,
      false,
      None,
      LaunchArgs { log_dir: Some(root.path().join("run")), ..midturn_launch_kwargs(run_id) },
      disabled_cgroup(),
    )
    .unwrap();
    bind_process(backend.midturn_state().expect("a gated Claude run has delivery state"));
    let events = Arc::new(Mutex::new(Vec::<Value>::new()));
    std::fs::create_dir_all(root.path().join("cwd")).unwrap();
    let mut request = RunRequest::new(
      "Initial prompt".to_string(),
      root.path().join("cwd"),
      AgentBaseEnvironment { values: charliebot_agents::gh_wrapper::stub_environment(root.path(), &stub_path(&stub)) },
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
      "cc-claude",
      Box::new(backend),
      LaunchArgs {
        extra_flags: Vec::new(),
        buffer_limit: None,
        instructions_content: None,
        instructions_file: None,
        resume_session_id: None,
        log_dir: Some(root.path().join("run")),
        cgroup_session_id: None,
        remote_launch: None,
        backend_kwargs: serde_json::Map::new(),
        manager_turn: false,
      },
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

  /// A CLI that reads one stream-json line, echoes it, and skips it as a duplicate when its transcript already
  /// holds the uuid. This is the behavior probe_dedupe.py measured on Claude Code 2.1.312 and later.
  /// A skipped line is echoed, marked completed, and the CLI then waits for the next stdin line instead of
  /// running a result.
  const DEDUPE_STUB: &str = concat!(
    "#!/bin/sh\n",
    "read -r line\n",
    "uuid=$(printf '%s' \"$line\" | sed -n 's/.*\"uuid\":\"\\([^\"]*\\)\".*/\\1/p')\n",
    "if [ -f \"$TRANSCRIPT\" ] && grep -qxF \"$uuid\" \"$TRANSCRIPT\"; then\n",
    "  printf '{\"type\":\"user\",\"isReplay\":true,\"uuid\":\"%s\",\"message\":{\"role\":\"user\"}}\\n' \"$uuid\"\n",
    "  printf '%s\\n' '{\"type\":\"command_lifecycle\",\"state\":\"completed\"}'\n",
    "  : > \"$SKIPPED_MARKER\"\n",
    "  cat > /dev/null\n",
    "  exit 0\n",
    "fi\n",
    "printf '%s\\n' \"$uuid\" >> \"$TRANSCRIPT\"\n",
    "printf '%s\\n' \"$line\" > \"$RAN_MARKER\"\n",
    "printf '{\"type\":\"user\",\"isReplay\":true,\"uuid\":\"%s\",\"message\":{\"role\":\"user\"}}\\n' \"$uuid\"\n",
    "printf '%s\\n' '{\"type\":\"result\",\"result\":\"ran\",\"usage\":{}}'\n",
  );

  async fn run_one_leg(
    root: &std::path::Path,
    stub: &std::path::Path,
    run_id: &str,
    prompt: &str,
    overlay: Env,
  ) -> charliebot_agents::runner::RunOutcome {
    let backend = ClaudeCodeBackend::from_launch(
      None,
      None,
      false,
      false,
      None,
      LaunchArgs {
        extra_flags: Vec::new(),
        buffer_limit: None,
        instructions_content: None,
        instructions_file: None,
        resume_session_id: None,
        log_dir: Some(root.join("run")),
        cgroup_session_id: Some("midturn-session".to_string()),
        remote_launch: None,
        backend_kwargs: serde_json::Map::from_iter([
          ("midturn_delivery".to_string(), Value::Bool(true)),
          ("midturn_run_id".to_string(), Value::String(run_id.to_string())),
          ("run_id".to_string(), Value::String(run_id.to_string())),
        ]),
        manager_turn: false,
      },
      disabled_cgroup(),
    )
    .unwrap();
    bind_process(backend.midturn_state().expect("a gated Claude run has delivery state"));
    std::fs::create_dir_all(root.join("cwd")).unwrap();
    let mut request = RunRequest::new(
      prompt.to_string(),
      root.join("cwd"),
      AgentBaseEnvironment { values: charliebot_agents::gh_wrapper::stub_environment(root, &stub_path(stub)) },
      SessionCgroupConfig::disabled(),
      Arc::new(Registry::new()),
    );
    request.cleanup_timeout = Duration::from_secs(2);
    request.environment_overlay = overlay;
    let runner = AgentRunner::new(
      "cc-claude",
      Box::new(backend),
      LaunchArgs {
        extra_flags: Vec::new(),
        buffer_limit: None,
        instructions_content: None,
        instructions_file: None,
        resume_session_id: None,
        log_dir: Some(root.join("run")),
        cgroup_session_id: None,
        remote_launch: None,
        backend_kwargs: serde_json::Map::new(),
        manager_turn: false,
      },
    );
    match tokio::time::timeout(Duration::from_secs(30), runner.run(request)).await {
      Ok(outcome) => outcome.unwrap(),
      Err(_) => {
        let _ = runner.terminate().await;
        panic!("the process produced no result; the CLI skipped its first line as a duplicate uuid");
      }
    }
  }

  /// Stages the dedupe CLI as `claude` under `root`. Returns its path and the environment of each leg, which points
  /// the stub at the transcript and at the markers that record each leg.
  fn stage_dedupe_cli(root: &std::path::Path) -> (std::path::PathBuf, Env) {
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let stub = bin.join("claude");
    charliebot_shell_harness::write_executable(&stub, DEDUPE_STUB);
    let overlay = Env::from([
      ("TRANSCRIPT".to_string(), root.join("transcript.jsonl").display().to_string()),
      ("SKIPPED_MARKER".to_string(), root.join("skipped").display().to_string()),
      ("RAN_MARKER".to_string(), root.join("ran").display().to_string()),
    ]);
    (stub, overlay)
  }

  /// The stream-json line the stub's last accepted leg received, so each leg is checked on its uuid and its prompt.
  fn received_line(root: &std::path::Path) -> Value {
    serde_json::from_str::<Value>(std::fs::read_to_string(root.join("ran")).unwrap().trim())
      .expect("the CLI received one stream-json line")
  }

  #[ignore = "integration"]
  // A relayed process receives its continuation prompt under a fresh uuid, so a CLI that skips duplicate uuids
  // still runs the turn to a result and the run finishes.
  #[tokio::test]
  async fn a_relayed_process_produces_a_result_when_the_cli_skips_duplicate_uuids() {
    let root = tempfile::tempdir().unwrap();
    let run_id = "11111111-2222-3333-4444-555555555564";
    let (stub, overlay) = stage_dedupe_cli(root.path());

    let first = run_one_leg(root.path(), &stub, run_id, "Initial prompt", overlay.clone()).await;
    assert_eq!(first.exit_code, Some(0));
    assert_eq!(received_line(root.path())["uuid"], run_id, "the first process received the Run id line");
    assert_eq!(received_line(root.path())["message"]["content"], "Initial prompt");

    // The relay binds the same Run again and hands the new process the continuation prompt, as the master loop does.
    let second = run_one_leg(root.path(), &stub, run_id, crate::claude_relay::CONTINUATION_PROMPT, overlay).await;
    assert_eq!(second.exit_code, Some(0), "the relayed process produced a result and the run finished");
    assert_eq!(
      received_line(root.path())["uuid"],
      initial_line_uuid(run_id, 1),
      "the relay's first line carries a uuid the transcript does not hold"
    );
    assert_eq!(received_line(root.path())["message"]["content"], crate::claude_relay::CONTINUATION_PROMPT);
    assert!(
      !root.path().join("skipped").exists(),
      "the CLI ran the continuation prompt instead of skipping it as a duplicate"
    );
    release_run(run_id);
  }

  // A server restart empties the Run's delivery state. The re-attach then seeds the first lines from the raw legs,
  // which hold the first line the first process echoed. The follow-up takes the next line, so the CLI runs it.
  #[ignore = "integration"]
  #[tokio::test]
  async fn a_follow_up_after_a_restart_takes_a_first_line_the_run_logs_do_not_hold() {
    let root = tempfile::tempdir().unwrap();
    let run_id = "11111111-2222-3333-4444-555555555565";
    let (stub, overlay) = stage_dedupe_cli(root.path());

    let first = run_one_leg(root.path(), &stub, run_id, "Initial prompt", overlay.clone()).await;
    assert_eq!(first.exit_code, Some(0));

    release_run(run_id);
    let follow_up = run_one_leg(root.path(), &stub, run_id, crate::claude_relay::CONTINUATION_PROMPT, overlay).await;
    assert_eq!(follow_up.exit_code, Some(0), "the follow-up produced a result after the restart");
    assert_eq!(
      received_line(root.path())["uuid"],
      initial_line_uuid(run_id, 1),
      "the follow-up's first line is one the run's logs and transcript do not hold"
    );
    assert!(!root.path().join("skipped").exists(), "the CLI ran the follow-up instead of skipping it");
    release_run(run_id);
  }
}
