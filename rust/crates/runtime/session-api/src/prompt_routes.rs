//! The prompt-inspection routes: the next-start preview of a task and the stored context of one past run
//! (`get_effective_prompt` and `get_run_context` in `src/runtime/api/sessions.py`).

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use charliebot_compat::responses;
use charliebot_compat::routing::get_only;
use charliebot_core::repo_root::RepoRoot;
use charliebot_core::text_repr::str_repr;
use charliebot_registry::FeatureContext;
use charliebot_store::metadata::TaskProfile;
use charliebot_tasks::prompts::{self, PromptEnv, TaskPromptError};
use serde_json::{Value, json};

use crate::route_support::store_error;

/// `EMPTY_BACKENDS_OPTIONS_REFUSAL` (`src/runtime/spawner_backends.py:46`): a preview refuses a config with no backend.
const EMPTY_BACKENDS_OPTIONS_REFUSAL: &str = "session backend resolution requires a configured backends.options entry";

/// The two prompt routes below `/api/sessions`.
pub(crate) fn routes(features: &Arc<FeatureContext>) -> Router {
  Router::new()
    .route("/api/sessions/{session_id}/effective-prompt", get_only(effective_prompt))
    .route("/api/sessions/{session_id}/runs/{run_id}/context", get_only(run_context))
    .with_state(Arc::clone(features))
}

/// Whether a path falls under one of the two prompt routes, for the trailing-slash redirect.
pub(crate) fn matches(path: &str) -> bool {
  let segments: Vec<&str> = path.split('/').collect();
  match segments.as_slice() {
    ["", "api", "sessions", session_id, "effective-prompt"] => !session_id.is_empty(),
    ["", "api", "sessions", session_id, "runs", run_id, "context"] => !session_id.is_empty() && !run_id.is_empty(),
    _ => false,
  }
}

/// `GET /api/sessions/{session_id}/effective-prompt`: the snapshot-shaped preview of the next start of a task node.
/// The checks run in the Python handler's order: the session, the run kind, the backend, then the assembly.
async fn effective_prompt(
  State(features): State<Arc<FeatureContext>>,
  Path(session_id): Path<String>,
  Query(params): Query<HashMap<String, String>>,
) -> Response {
  let meta = match features.store.require_session(&session_id).await {
    Ok(meta) => meta,
    Err(error) => return store_error(error),
  };
  let default_kind = match meta.profile {
    TaskProfile::Manager => "manager_turn",
    TaskProfile::Worker => "work",
  };
  // Python's `kind or default_kind`: an empty `kind` selects the default.
  let kind = match params.get("kind").filter(|kind| !kind.is_empty()) {
    Some(kind) if !is_run_kind(kind) => {
      return detail(StatusCode::BAD_REQUEST, format!("unknown run kind: {}", str_repr(kind)));
    }
    Some(kind) => kind.as_str(),
    None => default_kind,
  };
  let cfg = match features.config.get() {
    Ok(cfg) => cfg,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  // The preview resolves the backend as a launch does: a task without its own backend takes the first configured one.
  let option = if meta.backend.is_empty() {
    match cfg.backends.options.first() {
      Some(option) => option,
      None => return detail(StatusCode::BAD_REQUEST, EMPTY_BACKENDS_OPTIONS_REFUSAL),
    }
  } else {
    match cfg.get_backend_option(&meta.backend) {
      Some(option) => option,
      None => {
        return detail(
          StatusCode::BAD_REQUEST,
          format!(
            "session backend {} is not configured; update the task backend before previewing",
            str_repr(&meta.backend)
          ),
        );
      }
    }
  };
  let repo_root = match features.startup.require::<RepoRoot>() {
    Ok(repo_root) => repo_root,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  let env = PromptEnv { cfg: &cfg, registry: &features.registry, checkout: &repo_root };
  let coherent = match prompts::assemble_coherent_snapshot(&features.store, &env, &meta, kind, option).await {
    Ok(coherent) => coherent,
    Err(error) => {
      return match error.downcast_ref::<TaskPromptError>() {
        Some(error) => detail(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        None => responses::internal_error("RuntimeError", format!("{error:#}")),
      };
    }
  };
  let mut payload = json!({
    "session_id": session_id,
    "kind": kind,
    "mode": "preview",
    "overlay": {
      "declared": coherent.declared,
      "error": coherent.overlay_error.as_ref().map(|error| error.class_name()),
    },
  });
  // Python's `payload.update(snapshot)`: the snapshot's keys join the envelope.
  let Value::Object(snapshot) = coherent.snapshot.to_json_dict() else {
    unreachable!("a prompt snapshot serializes to an object")
  };
  payload.as_object_mut().expect("the preview envelope is an object").extend(snapshot);
  responses::json(StatusCode::OK, &payload)
}

/// `GET /api/sessions/{session_id}/runs/{run_id}/context`: one historical run's stored snapshot and its evidence.
async fn run_context(
  State(features): State<Arc<FeatureContext>>,
  Path((session_id, run_id)): Path<(String, String)>,
) -> Response {
  if let Err(error) = features.store.require_session(&session_id).await {
    return store_error(error);
  }
  match prompts::read_run_context(&features.store, &session_id, &run_id) {
    Ok(payload) => responses::json(StatusCode::OK, &payload),
    Err(error) => {
      let status = StatusCode::from_u16(error.status).expect("a context read error carries an HTTP status");
      detail(status, error.detail)
    }
  }
}

/// `src/infra/models.py` `RunKind`: the manager kinds and the worker kinds make up the literal together.
fn is_run_kind(kind: &str) -> bool {
  prompts::MANAGER_KINDS.contains(&kind) || prompts::WORKER_KINDS.contains(&kind)
}

/// FastAPI's `HTTPException(status, detail=text)` answer: the status and `{"detail": text}`.
fn detail(status: StatusCode, text: impl Into<String>) -> Response {
  responses::json(status, &json!({"detail": text.into()}))
}
