//! Review-chain reads: worker log summaries and reviewer backend selection.

use std::collections::HashSet;
use std::path::Path;

use charliebot_agents::backend_pool::BackendRoute;
use charliebot_agents::model_pool::Selector;
use charliebot_core::config::{BackendOption, CallClass, CallTarget, CharlieBotConfig};
use charliebot_core::event_types::MESSAGE_CONTENT_POINTER;
use charliebot_store::run_records::visit_events_from_end;
use serde_json::Value;

/// A selected backend, its resolved model, and the updated reviewer-attempt list.
pub type ReviewerBackendSelection = (String, Option<String>, Vec<String>);

/// A review selection or the diagnostic for a target with no remaining selectable backend.
#[derive(Debug, PartialEq, Eq)]
pub enum ReviewerBackendDecision {
  Selected(ReviewerBackendSelection),
  Unavailable { reason: String },
}

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

/// Select a reviewer using `backends.calls.review`, or the worker session's route when that call target is unset or
/// names `session`. Prefer a selectable backend the worker session has not used. If none is available, try the
/// worker's backend first, then another selectable member already used by the worker session. Never select a backend
/// that already failed this work run's review. Return a diagnostic when no candidate is selectable.
pub fn select_reviewer_backend(
  cfg: &CharlieBotConfig,
  selector: &Selector<'_>,
  scope: ReviewScope<'_>,
) -> anyhow::Result<ReviewerBackendDecision> {
  let ReviewScope { route, worker_backend, worker_model, tried_backends, used_backends } = scope;

  let target = match cfg.backends.calls.get(CallClass::Review) {
    Some(CallTarget::Backend(backend)) => Some(ReviewTarget::Backend(backend)),
    None | Some(CallTarget::Session) => {
      route.filter(|route| !route.backends.is_empty()).map(|route| ReviewTarget::Route(&route.backends))
    }
  };
  let mut excluded = vec![worker_backend.to_string()];
  excluded.extend(used_backends.iter().cloned());
  excluded.extend(tried_backends.iter().cloned());

  if let Some(target) = target
    && let Some(option) = target.select(selector, &excluded)?
  {
    return Ok(ReviewerBackendDecision::Selected(reviewer_selection(
      option,
      tried_backends,
      worker_backend,
      worker_model,
    )?));
  }

  if !tried_backends.iter().any(|backend| backend == worker_backend)
    && let Some(option) = selector.select(worker_backend, tried_backends)?
  {
    return Ok(ReviewerBackendDecision::Selected(reviewer_selection(
      option,
      tried_backends,
      worker_backend,
      worker_model,
    )?));
  }

  if let Some(target) = target
    && let Some(option) = target.select(selector, tried_backends)?
  {
    return Ok(ReviewerBackendDecision::Selected(reviewer_selection(
      option,
      tried_backends,
      worker_backend,
      worker_model,
    )?));
  }

  let reason = reviewer_unavailable_reason(cfg, selector, target, worker_backend, tried_backends)?;
  Ok(ReviewerBackendDecision::Unavailable { reason })
}

#[derive(Clone, Copy)]
enum ReviewTarget<'a> {
  Backend(&'a str),
  Route(&'a [String]),
}

impl ReviewTarget<'_> {
  fn select(self, selector: &Selector<'_>, excluded: &[String]) -> anyhow::Result<Option<BackendOption>> {
    match self {
      Self::Backend(backend) => selector.select(backend, excluded),
      Self::Route(backends) => selector.ordered(backends, excluded),
    }
  }

  fn members(self, cfg: &CharlieBotConfig) -> Vec<String> {
    let targets = match self {
      Self::Backend(backend) => vec![backend],
      Self::Route(backends) => backends.iter().map(String::as_str).collect(),
    };
    let mut members = Vec::new();
    let mut seen = HashSet::new();
    for target in targets {
      if let Some(pool) = cfg.backends.pool(target) {
        for member in &pool.members {
          if seen.insert(member.backend.as_str()) {
            members.push(member.backend.clone());
          }
        }
      } else if seen.insert(target) {
        members.push(target.to_string());
      }
    }
    members
  }
}

fn reviewer_selection(
  option: BackendOption,
  tried_backends: &[String],
  worker_backend: &str,
  worker_model: Option<&str>,
) -> anyhow::Result<ReviewerBackendSelection> {
  let configured_model = option.required_model("reviewer backend").map_err(anyhow::Error::msg)?;
  let model = if option.id == worker_backend {
    worker_model.map(str::to_string).unwrap_or(configured_model)
  } else {
    configured_model
  };
  let mut tried = tried_backends.to_vec();
  tried.push(option.id.clone());
  Ok((option.id, Some(model), tried))
}

fn reviewer_unavailable_reason(
  cfg: &CharlieBotConfig,
  selector: &Selector<'_>,
  target: Option<ReviewTarget<'_>>,
  worker_backend: &str,
  tried_backends: &[String],
) -> anyhow::Result<String> {
  let mut members = target.map_or_else(Vec::new, |target| target.members(cfg));
  let mut seen = members.iter().cloned().collect::<HashSet<_>>();
  if seen.insert(worker_backend.to_string()) {
    members.push(worker_backend.to_string());
  }
  let mut lines = Vec::with_capacity(members.len());
  for backend in members {
    let state = if tried_backends.iter().any(|tried| tried == &backend) {
      if backend == worker_backend {
        "failed reviewer (writer backend)".to_string()
      } else {
        "failed reviewer".to_string()
      }
    } else {
      let availability = selector.member_state_reason(&backend)?;
      if backend == worker_backend && availability == "not selectable" {
        "writer backend".to_string()
      } else if backend == worker_backend {
        format!("writer backend; {availability}")
      } else {
        availability
      }
    };
    lines.push(format!("- {backend}: {state}"));
  }
  Ok(format!("review target has no selectable backend:\n{}", lines.join("\n")))
}

/// Select a verify checker or cold reader with the call class's configured exclusions.
pub fn select_checker_backend(
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
