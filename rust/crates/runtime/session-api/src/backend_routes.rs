//! The backend-resolve route: the option a call class or a backend id runs on, as the server picks it.

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use charliebot_agents::call_resolve::{self, RESOLVE_PATH, ResolveError};
use charliebot_agents::model_pool::{Selector, turn_member};
use charliebot_compat::responses;
use charliebot_compat::routing::post_only;
use charliebot_core::config::{CallClass, CallTarget, yaml_mapping_to_json};
use charliebot_registry::FeatureContext;
use charliebot_registry::backend::CLAUDE_CONFIG_DIR_FIELD;
use charliebot_registry::caller::CallerIdentity;
use serde_json::Value;

use crate::route_support::store_error;

/// The resolve route over the shared feature context.
pub(crate) fn routes(features: &Arc<FeatureContext>) -> Router {
  Router::new().route(RESOLVE_PATH, post_only(resolve)).with_state(Arc::clone(features))
}

/// Whether a path is the resolve route, for the trailing-slash redirect.
pub(crate) fn matches(path: &str) -> bool {
  path == RESOLVE_PATH
}

/// The one key a resolve body names.
enum Target<'a> {
  Call(CallClass),
  Backend(&'a str),
}

fn target_of(body: &Value) -> Result<Target<'_>, String> {
  let object = body.as_object().ok_or_else(|| "the body is a JSON object naming call or backend".to_string())?;
  match (object.get("call"), object.get("backend"), object.len()) {
    (Some(Value::String(name)), None, 1) => match CallClass::from_key(name) {
      Some(class) => Ok(Target::Call(class)),
      None => {
        let keys = CallClass::ALL.map(CallClass::key).join(", ");
        Err(format!("unknown call class '{name}'; the keys are {keys}"))
      }
    },
    (None, Some(Value::String(id)), 1) => Ok(Target::Backend(id)),
    _ => Err("send exactly one of call or backend, as a string".to_string()),
  }
}

/// `POST /api/backends/resolve`: `{"call": "<class>"}` or `{"backend": "<id>"}` answers `{"option": <record>}`, the
/// option's `backends.options` entry without any key. A run token names the run's own member as the author, which an
/// `improve_review` call leaves out. An unset key, an unknown id or an empty pool answers 409 with the reason.
async fn resolve(
  State(features): State<Arc<FeatureContext>>,
  caller: CallerIdentity,
  Json(body): Json<Value>,
) -> Response {
  let target = match target_of(&body) {
    Ok(target) => target,
    Err(message) => return responses::http_exception_with_detail(StatusCode::BAD_REQUEST, &message),
  };
  let cfg = match features.config.get() {
    Ok(cfg) => cfg,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  let needs_author = match &target {
    Target::Call(class) => {
      *class == CallClass::ImproveReview || matches!(cfg.backends.calls.get(*class), Some(CallTarget::Session))
    }
    Target::Backend(_) => false,
  };
  let author = if needs_author {
    match caller.claims() {
      None => None,
      Some(claims) => {
        let meta = match features.store.require_session(&claims.session_id).await {
          Ok(meta) => meta,
          Err(error) => return store_error(error),
        };
        match turn_member(&features, &cfg, &meta) {
          Ok(member) => member,
          Err(error) => return responses::internal_error("RuntimeError", format!("{error:#}")),
        }
      }
    }
  } else {
    None
  };
  let selector = match Selector::from_features(&features, &cfg) {
    Ok(selector) => selector,
    Err(error) => return responses::internal_error("RuntimeError", format!("{error:#}")),
  };
  let resolved = match target {
    Target::Call(class) => call_resolve::resolve_call(&cfg, &selector, class, author.as_deref()),
    Target::Backend(id) => call_resolve::resolve_backend(&cfg, &selector, id),
  };
  let option = match resolved {
    Ok(option) => option,
    Err(ResolveError::Refused(message)) => {
      return responses::http_exception_with_detail(StatusCode::CONFLICT, &message);
    }
    Err(ResolveError::Failed(error)) => return responses::internal_error("RuntimeError", format!("{error:#}")),
  };
  let lifecycle = match features.registry.lifecycle_for(&option.backend_type) {
    Ok(lifecycle) => lifecycle,
    Err(error) => return responses::internal_error("RuntimeError", format!("{error:#}")),
  };
  let claude_config_dir = match lifecycle.resolve_account_dir(&option, &cfg) {
    Ok(Ok(dir)) => dir,
    Ok(Err(message)) => return responses::http_exception_with_detail(StatusCode::CONFLICT, &message),
    Err(error) => return responses::internal_error("RuntimeError", format!("{error:#}")),
  };
  match yaml_mapping_to_json(&option.record()) {
    Ok(record) => {
      let mut answer = serde_json::Map::new();
      answer.insert("option".to_string(), Value::Object(record));
      if let Some(dir) = claude_config_dir {
        answer.insert(CLAUDE_CONFIG_DIR_FIELD.to_string(), Value::String(dir));
      }
      responses::json(StatusCode::OK, &Value::Object(answer))
    }
    Err(error) => responses::internal_error("RuntimeError", error),
  }
}

#[cfg(test)]
mod tests {
  use charliebot_agents::backend_availability::BackendAvailability;
  use charliebot_agents::model_pool::RotationPositions;
  use charliebot_core::run_token::RunTokenClaims;
  use charliebot_store::metadata::{SessionMetadata, TaskProfile};
  use serde_json::json;

  use super::*;
  use crate::test_support::{GatedHome, answer, feature_context_with_auto};

  /// Two options, one with an endpoint and a credential, a rotate pool of both, and `judge` on the gateway option.
  const CONFIG: &str = "\
backends:
  options:
    - {id: synthetic-a, label: Synthetic A, type: synthetic, model: model-a, api_base: 'https://gateway.test', \
credential: gw}
    - {id: synthetic-b, label: Synthetic B, type: synthetic, model: model-b}
  pools:
    - {id: pool, label: Pool, rule: rotate, members: [synthetic-a, synthetic-b]}
  calls:
    judge: synthetic-a
    improve_review: pool
    review: session
";

  fn context_with(config: &str) -> (GatedHome, Arc<FeatureContext>) {
    let (home, features) = feature_context_with_auto(config);
    features.startup.insert(BackendAvailability::load(home.path()).unwrap()).unwrap();
    features.startup.insert(RotationPositions::default()).unwrap();
    (home, features)
  }

  async fn resolve_body(body: Value) -> (StatusCode, Value) {
    let (_home, features) = context_with(CONFIG);
    let caller = CallerIdentity::Operator { session_id: None };
    answer(resolve(State(features), caller, Json(body)).await).await
  }

  /// The resolve route answers the option's record for a call class or a backend id, with the credential's name and no
  /// key. An unset class, an unknown id or a malformed body answers with a detail that names what to change.
  #[tokio::test]
  async fn the_resolve_route_answers_the_record_and_refuses_what_it_cannot_pick() {
    let (status, body) = resolve_body(json!({"call": "judge"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["option"]["id"], "synthetic-a");
    assert_eq!(body["option"]["model"], "model-a");
    assert_eq!(body["option"]["credential"], "gw", "the record names the credential");
    assert!(body.get("claude_config_dir").is_none(), "{body}");
    assert!(!body.to_string().contains("api_key"), "{body}");

    let (status, body) = resolve_body(json!({"backend": "pool"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(["synthetic-a", "synthetic-b"].contains(&body["option"]["id"].as_str().unwrap()), "{body}");

    let (status, body) = resolve_body(json!({"call": "compaction"})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body["detail"].as_str().unwrap().contains("backends.calls.compaction is unset"), "{body}");

    let (status, body) = resolve_body(json!({"call": "review"})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body["detail"].as_str().unwrap().contains("backends.calls.review"), "{body}");

    let (status, body) = resolve_body(json!({"backend": "missing"})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body["detail"].as_str().unwrap().contains("'missing'"), "{body}");

    let (status, body) = resolve_body(json!({"call": "speculate"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["detail"].as_str().unwrap().contains("unknown call class 'speculate'"), "{body}");

    let (status, body) = resolve_body(json!({"call": "judge", "backend": "pool"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
  }

  /// A run token excludes the session's current member when resolving `improve_review`.
  #[tokio::test]
  async fn a_run_token_excludes_the_author_member_on_improve_review() {
    let (_home, features) = context_with(CONFIG);
    let mut session = SessionMetadata::new("author-session", "Author session", TaskProfile::Manager);
    session.backend = "synthetic-a".to_string();
    let metadata_path = features.store.metadata_path(&session.id);
    std::fs::create_dir_all(metadata_path.parent().unwrap()).unwrap();
    std::fs::write(&metadata_path, session.to_json_string(&[], true)).unwrap();
    let caller = CallerIdentity::Agent {
      claims: RunTokenClaims { session_id: "author-session".into(), run_id: "run-1".into(), agent: "worker".into() },
    };
    let (status, body) =
      answer(resolve(State(Arc::clone(&features)), caller, Json(json!({"call": "improve_review"}))).await).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["option"]["id"], "synthetic-b");
  }

  /// Resolving a backend id does not depend on the caller's session backend still being configured.
  #[tokio::test]
  async fn an_explicit_backend_resolution_does_not_read_the_callers_session_backend() {
    let (_home, features) = context_with(CONFIG);
    let mut session = SessionMetadata::new("stale-session", "Stale session", TaskProfile::Manager);
    session.backend = "removed".to_string();
    let metadata_path = features.store.metadata_path(&session.id);
    std::fs::create_dir_all(metadata_path.parent().unwrap()).unwrap();
    std::fs::write(&metadata_path, session.to_json_string(&[], true)).unwrap();
    let caller = CallerIdentity::Agent {
      claims: RunTokenClaims { session_id: session.id, run_id: "run-1".into(), agent: "worker".into() },
    };

    let (status, body) =
      answer(resolve(State(Arc::clone(&features)), caller, Json(json!({"backend": "synthetic-b"}))).await).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["option"]["id"], "synthetic-b");
  }
}
