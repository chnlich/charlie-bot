//! The Claude Code side of live user-message delivery for web manager turns (`src/runtime/midturn_delivery.py`).
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
    ("midturn_delivery".to_string(), Value::Bool(true)),
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
  use charliebot_registry::midturn_delivery::{bind_process, offer, release_run, translate_replay};
  use serde_json::{Value, json};

  use crate::backend::ClaudeCodeBackend;
  use charliebot_agents::agent_environment::AgentBaseEnvironment;
  use charliebot_agents::runner::{AgentRunner, RunRequest};
  use charliebot_agents::transport::cgroup::SessionCgroupConfig;

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
    let compact = delivered.iter().map(charliebot_store::ChatEvent::from_value).collect::<Vec<_>>();
    assert_eq!(
      charliebot_store::projection::events_to_messages(&compact, 0, Default::default()).unwrap(),
      Vec::<Value>::new()
    );
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
}
