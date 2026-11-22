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
use charliebot_core::config::{CallClass, yaml_mapping_to_json};
use charliebot_registry::FeatureContext;
use charliebot_registry::caller::CallerIdentity;
use serde_json::{Value, json};

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
    Err(message) => return detail(StatusCode::BAD_REQUEST, message),
  };
  let cfg = match features.config.get() {
    Ok(cfg) => cfg,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  let author = match caller.claims() {
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
  };
  let selector = match Selector::from_features(&features, &cfg) {
    Ok(selector) => selector,
    Err(error) => return responses::internal_error("RuntimeError", format!("{error:#}")),
  };
  let resolved = match target {
    Target::Call(class) => call_resolve::resolve_call(&cfg, &selector, class, author.as_deref()),
    Target::Backend(id) => call_resolve::resolve_backend(&cfg, &selector, id),
  };
  match resolved {
    Ok(option) => match yaml_mapping_to_json(&option.record()) {
      Ok(record) => responses::json(StatusCode::OK, &json!({"option": Value::Object(record)})),
      Err(error) => responses::internal_error("RuntimeError", error),
    },
    Err(ResolveError::Refused(message)) => detail(StatusCode::CONFLICT, message),
    Err(ResolveError::Failed(error)) => responses::internal_error("RuntimeError", format!("{error:#}")),
  }
}

fn detail(status: StatusCode, text: impl Into<String>) -> Response {
  responses::json(status, &json!({"detail": text.into()}))
}

#[cfg(test)]
mod tests {
  use charliebot_agents::backend_availability::BackendAvailability;
  use charliebot_agents::model_pool::RotationPositions;

  use super::*;
  use crate::test_support::{answer, feature_context_with_auto};

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
";

  async fn resolve_body(body: Value) -> (StatusCode, Value) {
    let (home, features) = feature_context_with_auto(CONFIG);
    features.startup.insert(BackendAvailability::load(home.path()).unwrap()).unwrap();
    features.startup.insert(RotationPositions::default()).unwrap();
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
    assert!(!body.to_string().contains("api_key"), "{body}");

    let (status, body) = resolve_body(json!({"backend": "pool"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(["synthetic-a", "synthetic-b"].contains(&body["option"]["id"].as_str().unwrap()), "{body}");

    let (status, body) = resolve_body(json!({"call": "compaction"})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body["detail"].as_str().unwrap().contains("backends.calls.compaction is unset"), "{body}");

    let (status, body) = resolve_body(json!({"backend": "missing"})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body["detail"].as_str().unwrap().contains("'missing'"), "{body}");

    let (status, body) = resolve_body(json!({"call": "speculate"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["detail"].as_str().unwrap().contains("unknown call class 'speculate'"), "{body}");

    let (status, body) = resolve_body(json!({"call": "judge", "backend": "pool"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
  }
}
