//! The dispatcher half of live user-message delivery for web manager turns (`src/runtime/midturn_delivery.py`).
//!
//! The delivery state itself — the serialized stdin writes, the replay acknowledgements and the stdin close — lives in
//! `charliebot_agents::backends::claude_code::midturn_delivery`; this module offers the pending inputs to the live Run,
//! records the acknowledgements on the manager item, and claims the echoed inputs at the finish.

use std::borrow::Cow;
use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::Context;
use charliebot_agents::backend_types;
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_core::event_types;
use charliebot_registry::backend::LaunchArgs;
use charliebot_registry::context::FeatureContext;
use charliebot_store::ChatEvent;
use charliebot_store::compact::CompactValue;
use charliebot_store::metadata::SessionMetadata;
use charliebot_store::run_records::{RAW_LOG_NAME, RunKind, RunRecord};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::dispatch::{Dispatcher, python_string};
use crate::task_input_prompt::compose_input_prompt;

/// The delivery-state module: the serialized stdin writes, the replay acknowledgements and the stdin close.
pub(crate) use charliebot_agents::backends::claude_code::midturn_delivery as claude_midturn;

/// One fresh whole-file translator: the mutable event projection of one raw-log scan.
pub type FreshTranslate = Box<dyn FnMut(Value) -> anyhow::Result<Vec<Value>> + Send>;

use claude_midturn::{is_gated_turn, offer as offer_write};

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

/// Add one replayed user id to the live manager item's web-reply binding (`midturn_delivery.record_ack`).
///
/// The manager queue carries the running ids by value, so an acknowledged input joins both the item and the queue's
/// binding. A reply posted while the turn runs reads the queue (`running_user_event_ids`), as it reads the live item
/// in Python.
pub fn record_ack(
  queue: &crate::master_queue::MasterQueue,
  session_id: &str,
  user_event_ids: &mut Vec<String>,
  item_run_id: &str,
  event: &Value,
) -> anyhow::Result<()> {
  if event.get("type").and_then(Value::as_str) != Some(event_types::INPUT_DELIVERED) {
    return Ok(());
  }
  let run_id = python_string(event.get("run_id"));
  if run_id != item_run_id {
    anyhow::bail!("input delivery for Run {run_id} reached manager Run {item_run_id}");
  }
  let input_id = python_string(event.get("input_event_id"));
  if !user_event_ids.contains(&input_id) {
    user_event_ids.push(input_id.clone());
  }
  queue.append_running_user_event_id(session_id, &input_id);
  Ok(())
}

/// The three fields of a raw transport line that the echo scan reads. A line whose field has another JSON type fails
/// to deserialize and is skipped, which the scan does too, since such a line cannot be a replayed input.
#[derive(Deserialize)]
struct ReplayLine<'a> {
  #[serde(rename = "type", borrow)]
  kind: Option<Cow<'a, str>>,
  #[serde(rename = "isReplay")]
  is_replay: Option<bool>,
  #[serde(borrow)]
  uuid: Option<Cow<'a, str>>,
}

/// Read non-initial replay uuids from every raw transport leg of this Run (`_echoed_input_ids`).
fn echoed_input_ids(transport_dir: &Path, run_id: &str) -> anyhow::Result<HashSet<String>> {
  if !transport_dir.is_dir() {
    // `run_dir.glob` over an absent transport directory matches nothing.
    return Ok(HashSet::new());
  }
  let mut raw_paths = std::fs::read_dir(transport_dir)
    .with_context(|| format!("reading the run transport {}", transport_dir.display()))?
    .filter_map(|entry| entry.ok())
    .map(|entry| entry.path())
    .filter(|path| {
      path.is_file()
        && path.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.starts_with(RAW_LOG_NAME))
    })
    .collect::<Vec<PathBuf>>();
  raw_paths.sort();
  let mut echoed = HashSet::new();
  let mut line = Vec::new();
  for raw_path in raw_paths {
    let raw_file =
      std::fs::File::open(&raw_path).with_context(|| format!("reading the raw log {}", raw_path.display()))?;
    let mut raw_lines = BufReader::new(raw_file);
    loop {
      line.clear();
      raw_lines.read_until(b'\n', &mut line).with_context(|| format!("reading the raw log {}", raw_path.display()))?;
      // Only complete lines count, like the byte-wise read the Python scan does: a line without its newline ends the
      // scan of this leg.
      let Some(complete) = line.strip_suffix(b"\n") else {
        break;
      };
      let Ok(event) = serde_json::from_slice::<ReplayLine<'_>>(complete) else {
        continue;
      };
      if event.kind.as_deref() != Some(event_types::USER) || event.is_replay != Some(true) {
        continue;
      }
      if let Some(input_id) = event.uuid.as_deref()
        && !input_id.is_empty()
        && input_id != run_id
      {
        echoed.insert(input_id.to_string());
      }
    }
  }
  Ok(echoed)
}

/// Claim only pending user events whose uuids the successful Run echoed (`claim_finish_inputs`).
///
/// The scan parses every raw line of the Run, so it rides a blocking thread like the round-notice projection of the
/// same files: on the async runtime's worker threads it would freeze every concurrent request and WebSocket at turn
/// end (`b03c38579`).
pub async fn claim_finish_inputs(
  dispatcher: &Dispatcher,
  meta: &SessionMetadata,
  run_id: &str,
  transport_dir: &Path,
  successful: bool,
) -> anyhow::Result<Vec<String>> {
  if !is_gated_turn(meta, true, RunKind::ManagerTurn) || !successful {
    return Ok(Vec::new());
  }
  let scan_run_id = run_id.to_string();
  let scan_dir = transport_dir.to_path_buf();
  let echoed = tokio::task::spawn_blocking(move || echoed_input_ids(&scan_dir, &scan_run_id))
    .await
    .context("the echoed raw-log scan task stopped before it finished")??;
  if echoed.is_empty() {
    return Ok(Vec::new());
  }
  Ok(dispatcher.claim_echoed_user_inputs(&meta.id, run_id, &echoed).await?)
}

/// Build the fresh raw-log translator used to follow or reconcile one Run (`midturn_delivery.fresh_translate`).
///
/// Stateful translates require one instance per scan. A gated web manager turn's replay lines need the gated backend's
/// translator, so the launch carries the stream-json arguments; a build failure degrades to the raw claude shape the
/// same way the worker side's reconcile translate does — never a crash of the follow.
pub fn fresh_translate(
  features: &FeatureContext,
  cfg: &CharlieBotConfig,
  option: Option<&BackendOption>,
  meta: &SessionMetadata,
  run_id: &str,
) -> anyhow::Result<FreshTranslate> {
  use claude_midturn::{CLAUDE_BACKEND_TYPES, launch_kwargs};
  let Some(option) = option else {
    return Ok(Box::new(|event| Ok(vec![event])));
  };
  let launch = if CLAUDE_BACKEND_TYPES.contains(&option.backend_type.as_str())
    && is_gated_turn(meta, true, RunKind::ManagerTurn)
  {
    let Some(midturn_run_id) = launch_kwargs(meta, true, RunKind::ManagerTurn, Some(run_id))? else {
      anyhow::bail!("a gated web manager turn needs its Run id");
    };
    LaunchArgs {
      backend_kwargs: Map::from_iter([
        ("midturn_delivery".to_string(), Value::Bool(true)),
        ("midturn_run_id".to_string(), Value::String(midturn_run_id)),
      ]),
      ..LaunchArgs::default()
    }
  } else {
    LaunchArgs::default()
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

  // Python: tests/test_claude_code_backend.py::test_midturn_offer_writes_only_web_user_messages_and_stops_at_result
  #[tokio::test]
  async fn midturn_offer_writes_only_web_user_messages_and_stops_at_result() {
    let home = tempfile::tempdir().unwrap();
    let features = test_fixtures::claude_features(&home.path().join("home"), &test_fixtures::claude_config(None));
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
    claude_midturn::bind_process(run_id, backend.midturn_run_id()).await.unwrap();
    claude_midturn::write_initial_prompt(
      Some(run_id.to_string()),
      Box::new(RecordingStdin(Arc::clone(&recorded))),
      Arc::new(|| false),
      "Initial prompt".to_string(),
    )
    .await
    .unwrap();

    let written = offer(&features, &meta, &run, &[user_event, agent_event]).await.unwrap();
    assert_eq!(written, [message_id.to_string()]);
    let lines = recorded.lock().expect("recording stdin mutex is not poisoned").clone();
    let lines = String::from_utf8(lines).unwrap();
    let lines = lines.lines().map(|line| serde_json::from_str::<Value>(line).unwrap()).collect::<Vec<_>>();
    assert_eq!(lines.iter().map(|line| line["uuid"].clone()).collect::<Vec<_>>(), [json!(run_id), json!(message_id)]);
    assert_eq!(lines[0]["message"]["content"], json!("Initial prompt"));
    assert_eq!(lines[1]["message"]["content"], json!("Please change the answer."));

    let mut thread_meta = SessionMetadata::new("thread", "Slack", TaskProfile::Manager);
    thread_meta.extra.insert("slack_origin".to_string(), json!({"team_id":"T","channel_id":"C","thread_ts":"1.0"}));
    assert_eq!(
      offer(&features, &thread_meta, &run, &[user_event_ref(message_id)]).await.unwrap(),
      Vec::<String>::new()
    );
    let mut discord_meta = SessionMetadata::new("discord-thread", "Discord", TaskProfile::Manager);
    discord_meta
      .extra
      .insert("discord_origin".to_string(), json!({"guild_id":"G","parent_channel_id":"C","thread_id":"1"}));
    assert_eq!(
      offer(&features, &discord_meta, &run, &[user_event_ref(message_id)]).await.unwrap(),
      Vec::<String>::new()
    );
    let worker_run = test_fixtures::run_record(&meta.id, run_id, RunKind::Work);
    assert_eq!(
      offer(&features, &meta, &worker_run, &[user_event_ref(message_id)]).await.unwrap(),
      Vec::<String>::new()
    );

    claude_midturn::on_backend_event(Some(run_id.to_string()), &json!({"type": "result"})).await.unwrap();
    let late = ChatEvent::from(json!({"id": "late-after-result", "type": event_types::USER, "content": "Too late."}));
    assert_eq!(offer(&features, &meta, &run, &[late]).await.unwrap(), Vec::<String>::new());
    assert_eq!(
      recorded.lock().expect("recording stdin mutex is not poisoned").iter().filter(|byte| **byte == b'\n').count(),
      2,
      "the late message stays queued"
    );
    claude_midturn::release_run(run_id);
  }

  fn user_event_ref(message_id: &str) -> ChatEvent {
    ChatEvent::from(json!({"id": message_id, "type": event_types::USER, "content": "Please change the answer."}))
  }

  #[test]
  fn echoed_ids_are_the_complete_non_initial_replays_of_every_raw_leg() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
      dir.path().join(RAW_LOG_NAME),
      concat!(
        "{\"type\":\"user\",\"isReplay\":true,\"uuid\":\"in-1\"}\n",
        "{\"type\":\"user\",\"isReplay\":true,\"uuid\":\"run-1\"}\n",
        "{\"type\":\"user\",\"isReplay\":false,\"uuid\":\"in-2\"}\n",
        "{\"type\":\"assistant\",\"isReplay\":true,\"uuid\":\"in-3\"}\n",
        "not json\n",
        "{\"type\":\"user\",\"isReplay\":true,\"uuid\":\"\"}\n",
        "{\"type\":\"user\",\"isReplay\":true,\"uuid\":5}\n",
        "{\"type\":\"user\",\"isReplay\":true,\"uuid\":\"in-4\"}\r\n",
        "{\"type\":\"user\",\"isReplay\":true,\"uuid\":\"in-6\"}",
      ),
    )
    .unwrap();
    std::fs::write(
      dir.path().join(format!("{RAW_LOG_NAME}.1")),
      "{\"type\":\"user\",\"isReplay\":true,\"uuid\":\"in-5\"}\n",
    )
    .unwrap();
    let echoed = echoed_input_ids(dir.path(), "run-1").unwrap();
    assert_eq!(echoed, HashSet::from(["in-1".to_string(), "in-4".to_string(), "in-5".to_string()]));
  }
}
