//! Review-chain reads: worker log summaries and reviewer backend selection.

use std::collections::HashSet;
use std::path::Path;

use charliebot_agents::backend_pool::BackendRoute;
use charliebot_agents::model_pool::Selector;
use charliebot_core::config::{CallClass, CallTarget, CharlieBotConfig};
use charliebot_core::event_types::MESSAGE_CONTENT_POINTER;
use charliebot_store::run_records::visit_events_from_end;
use serde_json::Value;

/// A selected backend, its resolved model, and the updated reviewer-attempt list.
pub type ReviewerBackendSelection = (String, Option<String>, Vec<String>);

/// Read the worker summary for review, logging two warning events when evidence is unavailable.
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

/// The worker-side facts that shape one reviewer selection: the worker session's route, its backend and model, and the
/// backends the worker session used or its reviewers already tried.
pub struct ReviewScope<'a> {
  pub route: Option<&'a BackendRoute>,
  pub worker_backend: &'a str,
  pub worker_model: Option<&'a str>,
  pub tried_backends: &'a [String],
  pub used_backends: &'a HashSet<String>,
}

/// Select the reviewer, verify checker, or cold reader that the call class `class` runs on. `backends.calls` names its
/// target: a pool selects a member the worker's backend and the used or tried backends do not hold. A model runs as
/// written; `session`, or no key, runs on the worker's own backend, or on the worker session's route. A pool with no
/// selectable member falls back to the worker's backend unless that backend was already tried, which ends the
/// selection with `None`.
///
/// `scope.route` is the worker session's recorded route. A review whose `backends.calls.review` is unset or `session`
/// runs on the route's pool, or on the route's ids in order. It falls back to the worker's backend when the route has
/// no member left. A route never overrides a `backends.calls.review` backend target: that target alone decides the
/// reviewer when the key is set.
pub fn select_reviewer_backend(
  cfg: &CharlieBotConfig,
  selector: &Selector<'_>,
  class: CallClass,
  scope: ReviewScope<'_>,
) -> anyhow::Result<Option<ReviewerBackendSelection>> {
  let ReviewScope { route, worker_backend, worker_model, tried_backends, used_backends } = scope;
  let caller = class.key();
  let worker_fallback = || {
    let mut tried = tried_backends.to_vec();
    tried.push(worker_backend.to_string());
    Some((worker_backend.to_string(), worker_model.map(str::to_string), tried))
  };
  let mut excluded = vec![worker_backend.to_string()];
  excluded.extend(used_backends.iter().cloned());
  excluded.extend(tried_backends.iter().cloned());
  if class == CallClass::Verify
    && let Some(CallTarget::Backend(target)) = cfg.backends.calls.get(class)
    && target == worker_backend
  {
    anyhow::bail!(
      "backends.calls.verify names '{target}', the backend of the session that starts the verify; name another model or a pool"
    );
  }
  match cfg.backends.calls.get(class) {
    None | Some(CallTarget::Session) => {
      let Some(route) = route.filter(|route| !route.backends.is_empty()) else {
        return Ok(worker_fallback());
      };
      // The router's ordered ids each pass through the same model-or-pool resolver. A route with no selectable member
      // falls back to the worker's backend, as a review with no route does.
      let picked = selector.ordered(&route.backends, &excluded)?;
      let Some(option) = picked else {
        return Ok(worker_fallback());
      };
      let model = option.required_model("route member").map_err(anyhow::Error::msg)?;
      return Ok(Some((option.id, Some(model), tried_backends.to_vec())));
    }
    Some(CallTarget::Backend(target)) => {
      if let Some(option) = selector.select(target, &excluded)? {
        let model = option.required_model("calls target").map_err(anyhow::Error::msg)?;
        let mut tried = tried_backends.to_vec();
        tried.push(option.id.clone());
        return Ok(Some((option.id, Some(model), tried)));
      }
    }
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
  Ok(worker_fallback())
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
  let blocks = event.pointer(MESSAGE_CONTENT_POINTER)?.as_array()?;
  Some(
    blocks
      .iter()
      .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
      .filter_map(|block| block.get("text").and_then(Value::as_str))
      .collect(),
  )
}
