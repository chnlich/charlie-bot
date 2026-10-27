//! Review-chain reads: worker log summaries and reviewer backend selection (`src/runtime/review.py`).

use std::collections::HashSet;
use std::path::Path;

use charliebot_agents::backend_pool::BackendPool;
use charliebot_core::config::CharlieBotConfig;
use serde_json::Value;

use crate::run_support::visit_events_from_end;

/// A selected backend, its resolved model, and the updated reviewer-attempt list.
pub type ReviewerBackendSelection = (String, Option<String>, Vec<String>);

/// Read the worker summary for review, logging Python's two warning events when evidence is unavailable.
pub async fn extract_review_context(session_id: &str, run_id: &str, worker_log_path: &Path) -> Option<String> {
  let session_id = session_id.to_string();
  let run_id = run_id.to_string();
  let worker_log_path = worker_log_path.to_path_buf();
  let result = tokio::task::spawn_blocking(move || read_worker_summary_file(&worker_log_path)).await;
  let worker_summary = match result {
    Ok(Ok(summary)) => summary,
    Ok(Err(error)) => {
      tracing::warn!(
        target: "charliebot_tasks",
        session = %session_id,
        thread = %run_id,
        error = %error,
        "review_context_worker_events_failed"
      );
      None
    }
    Err(error) => {
      tracing::warn!(
        target: "charliebot_tasks",
        session = %session_id,
        thread = %run_id,
        error = %error,
        "review_context_worker_events_failed"
      );
      None
    }
  };
  if worker_summary.as_deref().is_none_or(str::is_empty) {
    tracing::warn!(
      target: "charliebot_tasks",
      session = %session_id,
      thread = %run_id,
      "review_context_worker_summary_unavailable"
    );
    None
  } else {
    worker_summary
  }
}

/// Return the newest non-empty result or assistant text from parsed worker events.
pub fn worker_summary_from_events_log(events: &[Value]) -> Option<String> {
  for event in events.iter().rev() {
    match event.get("type").and_then(Value::as_str) {
      Some("result") => {
        if let Some(result) = event.get("result").and_then(Value::as_str).map(str::trim).filter(|text| !text.is_empty())
        {
          return Some(result.to_string());
        }
      }
      Some("assistant") => {
        if let Some(text) =
          assistant_event_text(event).map(|text| text.trim().to_string()).filter(|text| !text.is_empty())
        {
          return Some(text);
        }
      }
      _ => {}
    }
  }
  None
}

/// Return the newest non-empty message or content from an error event.
pub fn worker_error_from_events_log(events: &[Value]) -> Option<String> {
  events.iter().rev().find_map(|event| {
    if event.get("type").and_then(Value::as_str) != Some("error") {
      return None;
    }
    ["message", "content"].into_iter().find_map(|key| {
      event.get(key).and_then(Value::as_str).map(str::trim).filter(|text| !text.is_empty()).map(str::to_string)
    })
  })
}

/// Select a reviewer, verify checker, or cold reader from the shared backend pool.
pub fn select_reviewer_backend(
  cfg: &CharlieBotConfig,
  pool: &BackendPool,
  caller: &str,
  worker_backend: &str,
  worker_model: Option<&str>,
  tried_backends: &[String],
  used_backends: &HashSet<String>,
) -> anyhow::Result<Option<ReviewerBackendSelection>> {
  let mut excluded = vec![worker_backend.to_string()];
  excluded.extend(used_backends.iter().cloned());
  excluded.extend(tried_backends.iter().cloned());
  if let Some(option) = pool.select(cfg, caller, &excluded)? {
    let model = required_model(&option.model, "backends.pool entry ", &option.id)?;
    let mut tried = tried_backends.to_vec();
    tried.push(option.id.clone());
    return Ok(Some((option.id, Some(model), tried)));
  }
  if cfg.backends.pool.is_empty() {
    let mut tried = tried_backends.to_vec();
    tried.push(worker_backend.to_string());
    return Ok(Some((worker_backend.to_string(), worker_model.map(str::to_string), tried)));
  }
  if tried_backends.iter().any(|backend| backend == worker_backend) || used_backends.contains(worker_backend) {
    tracing::warn!(target: "charliebot_tasks", caller, tried = ?tried_backends, "reviewer_all_backends_exhausted");
    return Ok(None);
  }
  tracing::info!(
    target: "charliebot_tasks",
    caller,
    worker_backend,
    tried = ?tried_backends,
    "reviewer_fallback_to_worker_backend"
  );
  let mut tried = tried_backends.to_vec();
  tried.push(worker_backend.to_string());
  Ok(Some((worker_backend.to_string(), worker_model.map(str::to_string), tried)))
}

/// Return the newest worker summary directly from the run's events log, parsing only result and assistant events.
pub fn read_worker_summary_file(worker_log_path: &Path) -> anyhow::Result<Option<String>> {
  let (summary, _) = visit_events_from_end(worker_log_path, &["result", "assistant"], |event| {
    worker_summary_from_events_log(std::slice::from_ref(&event))
  })?;
  Ok(summary)
}

pub(crate) fn read_worker_error_file(worker_log_path: &Path) -> anyhow::Result<Option<String>> {
  let (error, _) = visit_events_from_end(worker_log_path, &["error"], |event| {
    worker_error_from_events_log(std::slice::from_ref(&event))
  })?;
  Ok(error)
}

fn assistant_event_text(event: &Value) -> Option<String> {
  let blocks = event.pointer("/message/content")?.as_array()?;
  Some(
    blocks
      .iter()
      .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
      .filter_map(|block| block.get("text").and_then(Value::as_str))
      .collect(),
  )
}

fn required_model(model: &str, subject: &str, backend_id: &str) -> anyhow::Result<String> {
  if model.is_empty() {
    anyhow::bail!("{subject}'{backend_id}' has no default model");
  }
  Ok(model.to_string())
}
