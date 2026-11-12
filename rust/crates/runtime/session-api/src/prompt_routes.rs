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

#[cfg(test)]
mod tests {
  use std::collections::HashMap;

  use charliebot_agents::backend_availability::{self, BackendAvailability};
  use charliebot_agents::model_pool::RotationPositions;
  use charliebot_store::{CreateTask, TaskCaller};
  use serde_json::Map;

  use super::*;
  use crate::test_support::{answer, feature_context_with_auto};

  /// A model `synthetic-a` and the rotate pool `auto` whose only member it is.
  const CONFIG: &str = "\
backends:
  options:
    - {id: synthetic-a, label: Synthetic A, type: synthetic, model: model-a}
  pools:
    - id: auto
      label: Auto
      rule: rotate
      members: [synthetic-a]
";

  /// The preview route as the web client calls it.
  async fn preview(features: &Arc<FeatureContext>, session_id: &str) -> (StatusCode, Value) {
    answer(effective_prompt(State(Arc::clone(features)), Path(session_id.to_string()), Query(HashMap::new())).await)
      .await
  }

  /// The repo root the prompt assembly reads its templates from.
  fn repo_root() -> charliebot_core::repo_root::RepoRoot {
    let checkout = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
      .ancestors()
      .nth(4)
      .expect("the repo root is an ancestor")
      .to_path_buf();
    charliebot_core::repo_root::RepoRoot::new(Some(checkout.as_os_str().to_os_string())).expect("repo root is valid")
  }

  /// Entrance #9 of the plan's entry-consistency item, success side: a pool session previews 200 from the member
  /// the next start runs on, the same 200 the model over it alone gets.
  #[tokio::test]
  async fn a_pool_session_previews_200_like_the_model_it_holds() {
    let (home, features) = feature_context_with_auto(CONFIG);
    charliebot_core::test_support::write_approval_policy_table(
      home.path(),
      charliebot_core::test_support::APPROVAL_POLICY_TABLE,
    );
    features.startup.insert(BackendAvailability::load(home.path()).unwrap()).unwrap();
    features.startup.insert(RotationPositions::default()).unwrap();
    features.startup.insert(repo_root()).unwrap();
    let mut session_ids = Vec::new();
    for (request_id, backend) in [("preview-model-ok", "synthetic-a"), ("preview-pool-ok", "auto")] {
      let created = features
        .store
        .create_task(
          CreateTask {
            request_id: request_id.to_string(),
            task_parent_id: None,
            profile: TaskProfile::Manager,
            task: None,
            name: Some(request_id.to_string()),
            backend: Some(backend.to_string()),
            group: None,
            session_id: None,
          },
          TaskCaller::System,
          Map::new(),
        )
        .await
        .unwrap();
      session_ids.push(created.id);
    }

    let (model_status, model) = preview(&features, &session_ids[0]).await;
    let (pool_status, pool) = preview(&features, &session_ids[1]).await;
    assert_eq!(model_status, StatusCode::OK, "{model}");
    assert_eq!(pool_status, StatusCode::OK, "{pool}");
    assert_eq!(pool["session_id"], session_ids[1]);
    assert_eq!(pool["mode"], "preview");
  }

  /// Entrance #9 of the plan's entry-consistency item, exhausted side: a cooling model and the pool over it preview
  /// the same exhaustion, so the preview keeps no pool-or-model branch of its own.
  #[tokio::test]
  async fn a_cooling_model_and_its_singleton_pool_preview_the_same_exhaustion() {
    let (home, features) = feature_context_with_auto(CONFIG);
    features.startup.insert(BackendAvailability::load(home.path()).unwrap()).unwrap();
    features.startup.insert(RotationPositions::default()).unwrap();
    let mut session_ids = Vec::new();
    for (request_id, backend) in [("preview-model", "synthetic-a"), ("preview-pool", "auto")] {
      let created = features
        .store
        .create_task(
          CreateTask {
            request_id: request_id.to_string(),
            task_parent_id: None,
            profile: TaskProfile::Manager,
            task: None,
            name: Some(request_id.to_string()),
            backend: Some(backend.to_string()),
            group: None,
            session_id: None,
          },
          TaskCaller::System,
          Map::new(),
        )
        .await
        .unwrap();
      session_ids.push(created.id);
    }
    backend_availability::backend_availability(&features)
      .unwrap()
      .record("synthetic-a", None, backend_availability::REASON_QUOTA_ERROR, "synthetic quota error")
      .unwrap();

    let (model_status, model) = preview(&features, &session_ids[0]).await;
    let (pool_status, pool) = preview(&features, &session_ids[1]).await;
    assert_eq!(model_status, StatusCode::BAD_REQUEST, "{model}");
    assert_eq!(pool_status, StatusCode::BAD_REQUEST, "{pool}");
    for body in [&model, &pool] {
      let detail = body["detail"].as_str().unwrap_or_default();
      assert!(detail.contains("- synthetic-a: quota_error"), "{detail}");
    }
  }
}
