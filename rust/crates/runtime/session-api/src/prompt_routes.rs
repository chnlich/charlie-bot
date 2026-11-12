//! The prompt-inspection route: the next-start preview of a task.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use charliebot_compat::responses;
use charliebot_compat::routing::get_only;
use charliebot_core::repo_root::RepoRoot;
use charliebot_core::run_kind::RunKind;
use charliebot_core::text_repr::str_repr;
use charliebot_registry::FeatureContext;
use charliebot_store::metadata::TaskProfile;
use charliebot_tasks::prompts::{self, PromptEnv, TaskPromptError};
use serde_json::{Value, json};

use crate::route_support::store_error;

/// `EMPTY_BACKENDS_OPTIONS_REFUSAL`: a preview refuses a config with no backend.
const EMPTY_BACKENDS_OPTIONS_REFUSAL: &str = "session backend resolution requires a configured backends.options entry";

/// The prompt route below `/api/sessions`.
pub(crate) fn routes(features: &Arc<FeatureContext>) -> Router {
  Router::new()
    .route("/api/sessions/{session_id}/effective-prompt", get_only(effective_prompt))
    .with_state(Arc::clone(features))
}

/// Whether a path falls under the prompt route, for the trailing-slash redirect.
pub(crate) fn matches(path: &str) -> bool {
  let segments: Vec<&str> = path.split('/').collect();
  match segments.as_slice() {
    ["", "api", "sessions", session_id, "effective-prompt"] => !session_id.is_empty(),
    _ => false,
  }
}

/// `GET /api/sessions/{session_id}/effective-prompt`: the snapshot-shaped preview of the next start of a task node.
/// The checks run in this order: the session, the run kind, the backend, then the assembly.
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
    TaskProfile::Manager => RunKind::ManagerTurn.as_str(),
    TaskProfile::Worker => RunKind::Work.as_str(),
  };
  // An empty `kind` selects the default.
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
  let mut preview_meta = meta.clone();
  if preview_meta.backend.is_empty() {
    let Some(backend) = cfg.backends.new_session_backend() else {
      return detail(StatusCode::BAD_REQUEST, EMPTY_BACKENDS_OPTIONS_REFUSAL);
    };
    preview_meta.backend = backend;
  }
  let selector = match charliebot_agents::model_pool::Selector::from_features(&features, &cfg) {
    Ok(selector) => selector,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  let backend_id = match charliebot_agents::model_pool::turn_member(&features, &cfg, &preview_meta) {
    Ok(Some(backend_id)) => backend_id,
    Ok(None) => match selector.exhausted_message(&preview_meta.backend) {
      Ok(message) => return detail(StatusCode::BAD_REQUEST, message),
      Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
    },
    Err(error) if cfg.backends.check_backend_id(&preview_meta.backend).is_err() => {
      return detail(StatusCode::BAD_REQUEST, error.to_string());
    }
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  let option =
    cfg.get_backend_option(&backend_id).expect("the model-pool resolver returns a configured backends.options id");
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
  // The snapshot's keys join the envelope.
  let Value::Object(snapshot) = coherent.snapshot.to_json_dict() else {
    unreachable!("a prompt snapshot serializes to an object")
  };
  payload.as_object_mut().expect("the preview envelope is an object").extend(snapshot);
  responses::json(StatusCode::OK, &payload)
}

/// The run kinds: the manager kinds and the worker kinds make up the literal together.
fn is_run_kind(kind: &str) -> bool {
  prompts::MANAGER_KINDS.contains(&kind) || prompts::WORKER_KINDS.contains(&kind)
}

/// The error answer: the status and `{"detail": text}`.
fn detail(status: StatusCode, text: impl Into<String>) -> Response {
  responses::json(status, &json!({"detail": text.into()}))
}
