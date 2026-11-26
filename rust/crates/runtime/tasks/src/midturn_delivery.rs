//! The dispatcher half of live user-message delivery for web manager turns.
//!
//! The delivery state lives in the runtime (`charliebot_registry::midturn_delivery`), and each backend writes into it
//! through its codec. This module offers the pending inputs to the live Run, records the acknowledgements on the
//! manager item, and claims the inputs the Run delivered, as its persisted `input_delivered` events name them, at the
//! finish.

use std::collections::HashSet;
use std::path::Path;

use charliebot_agents::backend_types;
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_core::event_types;
use charliebot_core::text_repr::field_text;
use charliebot_registry::backend::LaunchArgs;
use charliebot_registry::context::FeatureContext;
use charliebot_registry::midturn_delivery::{is_gated_turn, offer as offer_write};
use charliebot_store::ChatEvent;
use charliebot_store::compact::CompactValue;
use charliebot_store::metadata::SessionMetadata;
use charliebot_store::run_records::{RunKind, RunRecord};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value};

use crate::task_input_prompt::compose_input_prompt;

/// True for a manager session (`charliebot_registry::midturn_delivery::is_manager_session`).
pub use charliebot_registry::midturn_delivery::is_manager_session;

/// One fresh whole-file translator: the mutable event projection of one raw-log scan.
pub type FreshTranslate = Box<dyn FnMut(Value) -> anyhow::Result<Vec<Value>> + Send>;

/// Write each eligible pending web-user event once to the live Run (`midturn_delivery.offer`).
pub async fn offer(
  features: &FeatureContext,
  meta: &SessionMetadata,
  run: &RunRecord,
  pending: &[ChatEvent],
) -> anyhow::Result<Vec<String>> {
  if !is_gated_turn(meta, run.kind == RunKind::ManagerTurn, run.kind) {
    return Ok(Vec::new());
  }
  let mut inputs = Vec::new();
  for event in pending {
    if event.get("type").and_then(CompactValue::as_str) != Some(event_types::USER) {
      continue;
    }
    let (content, _uploads) = compose_input_prompt(features, meta, std::slice::from_ref(event), &Map::new())?;
    let id = event.get("id").and_then(CompactValue::as_str).map_or("None".to_string(), str::to_string);
    inputs.push((id, content));
  }
  offer_write(&run.id, inputs).await
}

/// Add one replayed user id to the live manager item's input batch (`midturn_delivery.record_ack`).
///
/// The item's batch becomes the turn's `master_done` input ids, while the run record holds the receipts that the
/// web-reply readers consult.
pub fn record_ack(user_event_ids: &mut Vec<String>, item_run_id: &str, event: &Value) -> anyhow::Result<()> {
  if event.get("type").and_then(Value::as_str) != Some(event_types::INPUT_DELIVERED) {
    return Ok(());
  }
  let run_id = field_text(event.get("run_id"));
  if run_id != item_run_id {
    anyhow::bail!("input delivery for Run {run_id} reached manager Run {item_run_id}");
  }
  let input_id = field_text(event.get(event_types::INPUT_EVENT_ID));
  if !user_event_ids.contains(&input_id) {
    user_event_ids.push(input_id);
  }
  Ok(())
}

/// The dispatcher operation that binds delivered inputs to a Run.
pub trait ClaimEchoedInputs: Send + Sync {
  /// Append the still-pending USER events that the launched manager Run echoed.
  fn claim_echoed_user_inputs<'a>(
    &'a self,
    session_id: &'a str,
    run_id: &'a str,
    echoed_ids: &'a HashSet<String>,
  ) -> BoxFuture<'a, anyhow::Result<Vec<String>>>;
}

/// The input ids mid-turn delivery handed one Run, read from the Run's persisted `input_delivered` events. The finish
/// claim and the close check both read them, so a delivery counts once the runtime has recorded it.
pub fn delivered_input_ids(facts: &[ChatEvent], run_id: &str) -> HashSet<String> {
  facts
    .iter()
    .filter(|event| event.get("type").and_then(CompactValue::as_str) == Some(event_types::INPUT_DELIVERED))
    .filter(|event| event.get("run_id").and_then(CompactValue::as_str) == Some(run_id))
    .filter_map(|event| event.get(event_types::INPUT_EVENT_ID).and_then(CompactValue::as_str).map(str::to_string))
    .collect()
}

/// Claim only pending user events whose ids the successful Run delivered (`claim_finish_inputs`).
pub async fn claim_finish_inputs(
  dispatcher: &dyn ClaimEchoedInputs,
  meta: &SessionMetadata,
  run_id: &str,
  delivered: HashSet<String>,
  successful: bool,
) -> anyhow::Result<Vec<String>> {
  if !is_gated_turn(meta, true, RunKind::ManagerTurn) || !successful || delivered.is_empty() {
    return Ok(Vec::new());
  }
  dispatcher.claim_echoed_user_inputs(&meta.id, run_id, &delivered).await
}

/// Build the fresh raw-log translator used to follow or reconcile one Run (`midturn_delivery.fresh_translate`).
///
/// Stateful translates require one instance per scan. A gated web manager turn's replay lines need the gated backend's
/// translator, so the launch carries the mid-turn kwargs its lifecycle returns. A build failure passes each raw event
/// through unchanged, the same way the worker side's reconcile translate does — never a crash of the follow.
pub fn fresh_translate(
  features: &FeatureContext,
  cfg: &CharlieBotConfig,
  option: Option<&BackendOption>,
  meta: &SessionMetadata,
  run_id: &str,
  log_dir: Option<&Path>,
) -> anyhow::Result<FreshTranslate> {
  let Some(option) = option else {
    return Ok(Box::new(|event| Ok(vec![event])));
  };
  let launch = match features.registry.lifecycle_for(&option.backend_type) {
    Ok(lifecycle) => {
      let backend_kwargs = lifecycle.midturn_launch_kwargs(meta, true, RunKind::ManagerTurn, Some(run_id))?;
      LaunchArgs { backend_kwargs, log_dir: log_dir.map(Path::to_path_buf), ..LaunchArgs::default() }
    }
    // A type no package registered has no lifecycle and no mid-turn input path; the build below reports it.
    Err(_) => LaunchArgs::default(),
  };
  match backend_types::build_backend(std::sync::Arc::clone(&features.registry), option, cfg, launch) {
    Ok(built) => {
      let mut translator = built.backend.translator();
      Ok(Box::new(move |event| translator.translate(event)))
    }
    Err(error) => {
      tracing::warn!(target: "charliebot_tasks", backend = %option.id, error = %error, "master_cc_resume_translate_unresolved");
      Ok(Box::new(|event| Ok(vec![event])))
    }
  }
}

#[cfg(test)]
mod tests {
  use std::sync::{Arc, Mutex};

  use charliebot_agents::backend_types;
  use charliebot_registry::backend::LaunchArgs;
  use charliebot_registry::midturn_delivery as claude_midturn;
  use charliebot_store::metadata::{SessionMetadata, TaskProfile};
  use charliebot_store::run_records::RunKind;
  use serde_json::json;
  use tokio::io::AsyncWrite;

  use crate::test_fixtures;

  use super::*;

  struct RecordingStdin(Arc<Mutex<Vec<u8>>>);

  impl AsyncWrite for RecordingStdin {
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

  #[tokio::test]
  async fn midturn_offer_writes_manager_user_messages_and_stops_at_result() {
    charliebot_registry::thread_origin::register_origin_field("slack", "slack_origin");
    charliebot_registry::thread_origin::register_origin_field("discord", "discord_origin");
    charliebot_registry::thread_origin::register_origin_field("feishu", "feishu_origin");
    let home = tempfile::tempdir().unwrap();
    let features = test_fixtures::features_with_packages(
      &home.path().join("home"),
      &test_fixtures::claude_config(None),
      &[
        ("backends.claude_code", charliebot_claude_code::register),
        ("features.web_reply", charliebot_web_reply::register),
      ],
    );
    let cfg = features.config.get().unwrap();
    let option = cfg.get_backend_option("fable").unwrap().clone();
    let run_id = "11111111-2222-3333-4444-555555555552";
    let message_id = "22222222-2222-3333-4444-555555555552";
    let backend = {
      let launch = LaunchArgs {
        backend_kwargs: serde_json::Map::from_iter([
          ("run_kind".to_string(), json!("work")),
          ("midturn_delivery".to_string(), json!(true)),
          ("midturn_run_id".to_string(), json!(run_id)),
        ]),
        ..LaunchArgs::default()
      };
      backend_types::build_backend(Arc::clone(&features.registry), &option, &cfg, launch).unwrap().backend
    };
    let meta = SessionMetadata::new("session", "Web", TaskProfile::Manager);
    let run = test_fixtures::run_record(&meta.id, run_id, RunKind::ManagerTurn);
    let user_event =
      ChatEvent::from(json!({"id": message_id, "type": event_types::USER, "content": "Please change the answer."}));
    let agent_event =
      ChatEvent::from(json!({"id": "agent-1", "type": event_types::AGENT_MESSAGE, "content": "Keep this queued."}));
    let recorded = Arc::new(Mutex::new(Vec::<u8>::new()));
    let state = backend.midturn_state().expect("a gated Claude run has delivery state");
    std::fs::create_dir_all(home.path().join("run")).unwrap();
    state.set_run_dir(&home.path().join("run")).unwrap();
    state
      .write_prompt(Box::new(RecordingStdin(Arc::clone(&recorded))), Arc::new(|| false), "Initial prompt")
      .await
      .unwrap();
    claude_midturn::bind_process(Arc::clone(&state));

    let written = offer(&features, &meta, &run, &[user_event, agent_event]).await.unwrap();
    assert_eq!(written, [message_id.to_string()]);
    let lines = recorded.lock().expect("recording stdin mutex is not poisoned").clone();
    let lines = String::from_utf8(lines).unwrap();
    let lines = lines.lines().map(|line| serde_json::from_str::<Value>(line).unwrap()).collect::<Vec<_>>();
    assert_eq!(lines.iter().map(|line| line["uuid"].clone()).collect::<Vec<_>>(), [json!(run_id), json!(message_id)]);
    assert_eq!(lines[0]["message"]["content"], json!("Initial prompt"));
    assert_eq!(lines[1]["message"]["content"], json!("[User message]\nPlease change the answer."));

    // A manager bound to a chat thread takes live input as a web manager does, each platform's thread alike.
    let thread_metas = [
      (
        "thread",
        "slack_origin",
        json!({"team_id":"T","channel_id":"C","thread_ts":"1.0"}),
        "33333333-2222-3333-4444-555555555552",
      ),
      (
        "discord-thread",
        "discord_origin",
        json!({"guild_id":"G","parent_channel_id":"C","thread_id":"1"}),
        "33333333-2222-3333-4444-555555555553",
      ),
      (
        "feishu-thread",
        "feishu_origin",
        json!({"tenant_key":"T","chat_id":"oc_synthetic","root_id":"om_synthetic"}),
        "33333333-2222-3333-4444-555555555554",
      ),
    ];
    // The initial prompt, the web message and one message per thread manager are each one stdin line.
    let written_lines = 2 + thread_metas.len();
    for (session_id, origin_field, origin, thread_message_id) in thread_metas {
      let mut thread_meta = SessionMetadata::new(session_id, "Thread", TaskProfile::Manager);
      thread_meta.extra.insert(origin_field.to_string(), origin);
      assert_eq!(
        offer(&features, &thread_meta, &run, &[user_event_ref(thread_message_id)]).await.unwrap(),
        [thread_message_id.to_string()],
        "{origin_field}"
      );
    }
    let worker_run = test_fixtures::run_record(&meta.id, run_id, RunKind::Work);
    assert_eq!(
      offer(&features, &meta, &worker_run, &[user_event_ref(message_id)]).await.unwrap(),
      Vec::<String>::new()
    );

    state.observe(&json!({"type": "result"})).unwrap();
    state.settle_turn().await.unwrap();
    let late = ChatEvent::from(json!({"id": "late-after-result", "type": event_types::USER, "content": "Too late."}));
    assert_eq!(offer(&features, &meta, &run, &[late]).await.unwrap(), Vec::<String>::new());
    assert_eq!(
      recorded.lock().expect("recording stdin mutex is not poisoned").iter().filter(|byte| **byte == b'\n').count(),
      written_lines,
      "the late message stays queued"
    );
    claude_midturn::release_run(run_id);
  }

  fn user_event_ref(message_id: &str) -> ChatEvent {
    ChatEvent::from(json!({"id": message_id, "type": event_types::USER, "content": "Please change the answer."}))
  }
}
