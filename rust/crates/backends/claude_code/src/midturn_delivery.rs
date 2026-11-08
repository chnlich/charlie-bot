//! The Claude Code side of live user-message delivery for web manager turns.
//!
//! The delivery state — the serialized stdin writes, the replay acknowledgements and the stdin close — lives in the
//! runtime (`charliebot_registry::midturn_delivery`), which both this backend and the task queue use. This module
//! holds the Claude-specific part: the stream-json launch arguments that turn the path on, which the lifecycle returns
//! to the runtime (`BackendLifecycle::midturn_launch_kwargs`), and the replay translator.

use charliebot_registry::backend::{LaunchContext, LaunchKind, Translate};
use charliebot_store::metadata::SessionMetadata;
use charliebot_store::run_records::RunKind;
use serde_json::{Map, Value};

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
    ("midturn_run_id".to_string(), Value::String(run_id)),
  ]))
}

/// Insert the stream-json launch arguments into `backend_kwargs` when `ctx` is a gated web manager turn.
pub fn insert_launch_kwargs(backend_kwargs: &mut Map<String, Value>, ctx: &LaunchContext) -> anyhow::Result<()> {
  let meta = ctx.session_meta.lock().expect("launch metadata mutex is not poisoned");
  let arguments = launch_kwargs(&meta, ctx.kind == LaunchKind::Turn, ctx.run_kind, ctx.run_id.as_deref())?;
  backend_kwargs.extend(arguments);
  Ok(())
}

/// The gated translator: replays of delivered inputs become `input_delivered` events.
pub struct MidturnTranslate {
  pub(crate) run_id: Option<String>,
}

impl Translate for MidturnTranslate {
  fn translate(&mut self, raw: Value) -> anyhow::Result<Vec<Value>> {
    charliebot_registry::midturn_delivery::translate_replay(self.run_id.as_deref(), &raw)
  }
}

#[cfg(test)]
mod tests {
  use std::sync::{Arc, Mutex};
  use std::time::Duration;

  use charliebot_registry::Registry;
  use charliebot_registry::backend::{AgentBackend, Env, LaunchArgs};
  use charliebot_registry::midturn_delivery::{bind_process, initial_line_uuid, offer, release_run, translate_replay};
  use serde_json::{Value, json};

  use crate::backend::ClaudeCodeBackend;
  use charliebot_agents::agent_environment::AgentBaseEnvironment;
  use charliebot_agents::runner::{AgentRunner, RunRequest};
  use charliebot_agents::transport::cgroup::SessionCgroupConfig;

  fn disabled_cgroup() -> SessionCgroupConfig {
    SessionCgroupConfig::disabled()
  }

  /// An agent environment whose PATH resolves `gh` to a stub wrapper, as every agent spawn requires.
  fn gh_wrapper_env(root: &std::path::Path) -> Env {
    use std::os::unix::fs::PermissionsExt;
    let wrapper_dir = root.join("gh-wrapper");
    std::fs::create_dir_all(&wrapper_dir).unwrap();
    let wrapper = wrapper_dir.join("gh");
    std::fs::write(&wrapper, "#!/bin/sh\n").unwrap();
    let mut permissions = std::fs::metadata(&wrapper).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&wrapper, permissions).unwrap();
    Env::from([
      ("PATH".to_string(), format!("{}:/usr/bin:/bin", wrapper_dir.display())),
      (charliebot_agents::gh_wrapper::WRAPPER_PATH_VAR.to_string(), wrapper.display().to_string()),
    ])
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
    let compact = delivered.iter().map(charliebot_store::ChatEvent::from_value).collect::<Vec<_>>();
    assert_eq!(
      charliebot_store::projection::events_to_messages(&compact, 0, Default::default()).unwrap(),
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
    let stub = root.path().join("claude-stream-json-stub");
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
      Some(stub.display().to_string()),
      false,
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
      AgentBaseEnvironment { values: gh_wrapper_env(root.path()) },
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

  /// A CLI that reads one stream-json line, echoes it, and skips it as a duplicate when its transcript already
  /// holds the uuid — the behavior probe_dedupe.py measured on Claude Code 2.1.312 and later. A skipped line is
  /// echoed, marked completed, and the CLI then waits for the next stdin line instead of running a result.
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
      Some(stub.display().to_string()),
      false,
      false,
      None,
      LaunchArgs {
        log_dir: Some(root.join("run")),
        backend_kwargs: serde_json::Map::from_iter([
          ("midturn_delivery".to_string(), Value::Bool(true)),
          ("midturn_run_id".to_string(), Value::String(run_id.to_string())),
        ]),
        ..LaunchArgs::default()
      },
      disabled_cgroup(),
    )
    .unwrap();
    bind_process(run_id, backend.midturn_run_id()).await.unwrap();
    std::fs::create_dir_all(root.join("cwd")).unwrap();
    let mut request = RunRequest::new(
      prompt.to_string(),
      root.join("cwd"),
      AgentBaseEnvironment { values: gh_wrapper_env(root) },
      SessionCgroupConfig::disabled(),
      Arc::new(Registry::new()),
    );
    request.cleanup_timeout = Duration::from_secs(2);
    request.environment_overlay = overlay;
    let runner = AgentRunner::new(
      "cc-claude",
      Box::new(backend),
      LaunchArgs { log_dir: Some(root.join("run")), ..LaunchArgs::default() },
    );
    match tokio::time::timeout(Duration::from_secs(30), runner.run(request)).await {
      Ok(outcome) => outcome.unwrap(),
      Err(_) => {
        let _ = runner.terminate().await;
        panic!("the process produced no result; the CLI skipped its first line as a duplicate uuid");
      }
    }
  }

  #[ignore = "integration"]
  // A relayed process receives its continuation prompt under a fresh uuid, so a CLI that skips duplicate uuids
  // still runs the turn to a result and the run finishes.
  #[tokio::test]
  async fn a_relayed_process_produces_a_result_when_the_cli_skips_duplicate_uuids() {
    let root = tempfile::tempdir().unwrap();
    let run_id = "11111111-2222-3333-4444-555555555564";
    let transcript = root.path().join("transcript.jsonl");
    let skipped = root.path().join("skipped");
    let ran = root.path().join("ran");
    let stub = root.path().join("claude-dedupe-stub");
    charliebot_shell_harness::write_executable(&stub, DEDUPE_STUB);
    let overlay = || {
      Env::from([
        ("TRANSCRIPT".to_string(), transcript.display().to_string()),
        ("SKIPPED_MARKER".to_string(), skipped.display().to_string()),
        ("RAN_MARKER".to_string(), ran.display().to_string()),
      ])
    };

    // The stub records the stream-json line it received, so each leg is checked on its uuid and its prompt.
    let received = || {
      serde_json::from_str::<Value>(std::fs::read_to_string(&ran).unwrap().trim())
        .expect("the CLI received one stream-json line")
    };

    let first = run_one_leg(root.path(), &stub, run_id, "Initial prompt", overlay()).await;
    assert_eq!(first.exit_code, Some(0));
    assert_eq!(received()["uuid"], run_id, "the first process received the Run id line");
    assert_eq!(received()["message"]["content"], "Initial prompt");

    // The relay binds the same Run again and hands the new process the continuation prompt, as the master loop does.
    let second = run_one_leg(root.path(), &stub, run_id, crate::claude_relay::CONTINUATION_PROMPT, overlay()).await;
    assert_eq!(second.exit_code, Some(0), "the relayed process produced a result and the run finished");
    assert_eq!(
      received()["uuid"],
      initial_line_uuid(run_id, 1),
      "the relay's first line carries a uuid the transcript does not hold"
    );
    assert_eq!(received()["message"]["content"], crate::claude_relay::CONTINUATION_PROMPT);
    assert!(!skipped.exists(), "the CLI ran the continuation prompt instead of skipping it as a duplicate");
    release_run(run_id);
  }
}
