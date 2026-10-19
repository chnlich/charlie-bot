//! `GET /api/sessions/{session_id}/plans`: the session's plan registry (`src/features/artifacts/api.py` `list_plans`).

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::Response;
use charliebot_compat::responses;
use charliebot_compat::routing::get_only;
use charliebot_registry::FeatureContext;
use serde_json::json;

use crate::plans::{self, PLANS_FILE};

/// The route table, built with the shared feature context.
pub(crate) fn routes(features: &Arc<FeatureContext>) -> Router {
  Router::new().route("/api/sessions/{session_id}/plans", get_only(list_plans)).with_state(Arc::clone(features))
}

/// Whether a path is the plans route, for the server's trailing-slash redirect.
pub(crate) fn matches(path: &str) -> bool {
  let Some(rest) = path.strip_prefix("/api/sessions/") else {
    return false;
  };
  matches!(rest.split_once('/'), Some((session_id, "plans")) if !session_id.is_empty())
}

/// The plan registry of a session with derived states and read errors. A missing session answers 404; a corrupt
/// registry answers 200 with its error entries.
async fn list_plans(State(features): State<Arc<FeatureContext>>, AxumPath(session_id): AxumPath<String>) -> Response {
  if let Some(response) = require_session(&features, &session_id).await {
    return response;
  }
  let plans_path = features.store.session_dir(&session_id).join(PLANS_FILE);
  let read_session = session_id.clone();
  let read = match tokio::task::spawn_blocking(move || plans::read_plans_tolerant(&plans_path, &read_session)).await {
    Ok(read) => read,
    Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
  };
  match read {
    Ok(read) => responses::fast_json(StatusCode::OK, &read.into_json()),
    Err(error) => responses::internal_error(error.kind, error.detail),
  }
}

/// The `require_session` dependency: the response that refuses the request when the session is missing, else `None`.
async fn require_session(features: &FeatureContext, session_id: &str) -> Option<Response> {
  match features.store.require_session(session_id).await {
    Ok(_) => None,
    Err(error) if error.status_code() == Some(404) => {
      Some(responses::json(StatusCode::NOT_FOUND, &json!({"detail": error.to_string()})))
    }
    Err(error) => Some(responses::internal_error("RuntimeError", error.to_string())),
  }
}

#[cfg(test)]
mod tests {
  use std::fs;

  use axum::body::{Body, to_bytes};
  use axum::http::Request;
  use charliebot_store::metadata::{SessionMetadata, TaskProfile};
  use serde_json::Value;
  use tower::ServiceExt;

  use super::*;
  use crate::test_support::features;

  /// Writes a root session through the registered slots, so its metadata file reads back as the store writes it.
  fn write_session(features: &FeatureContext, session_id: &str) {
    let slots = features.registry.metadata_slots();
    let meta = SessionMetadata::new(session_id, session_id, TaskProfile::Manager);
    let path = features.store.metadata_path(session_id);
    fs::create_dir_all(path.parent().expect("a session file has a directory"))
      .expect("the session directory is created");
    fs::write(path, meta.to_json_string(&slots, true)).expect("the metadata is written");
  }

  /// The status and the raw body of one GET to the plans route.
  async fn get(features: &Arc<FeatureContext>, uri: &str) -> (StatusCode, Vec<u8>) {
    let request = Request::builder().uri(uri).body(Body::empty()).expect("the request builds");
    let response = routes(features).oneshot(request).await.expect("the router is infallible");
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.expect("the body reads");
    (status, body.to_vec())
  }

  #[tokio::test]
  async fn an_unknown_session_answers_404_with_the_session_detail() {
    let (_home, features) = features();
    let (status, body) = get(&features, "/api/sessions/no-such/plans").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
      serde_json::from_slice::<Value>(&body).expect("the body is JSON"),
      json!({"detail": "Session not found"})
    );
  }

  #[tokio::test]
  async fn a_corrupt_registry_answers_200_with_one_error_entry() {
    let (_home, features) = features();
    write_session(&features, "sess");
    fs::write(features.store.session_dir("sess").join(PLANS_FILE), "{not valid json").expect("the registry is written");
    let (status, body) = get(&features, "/api/sessions/sess/plans").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
      serde_json::from_slice::<Value>(&body).expect("the body is JSON"),
      json!({
        "plans": [],
        "errors": [{
          "session_id": "sess",
          "plan_id": null,
          "error": "Expecting property name enclosed in double quotes: line 1 column 2 (char 1)"
        }]
      })
    );
  }

  #[tokio::test]
  async fn a_session_without_a_registry_answers_no_plans() {
    let (_home, features) = features();
    write_session(&features, "sess");
    assert_eq!(
      get(&features, "/api/sessions/sess/plans").await,
      (StatusCode::OK, br#"{"plans":[],"errors":[]}"#.to_vec())
    );
  }

  #[tokio::test]
  async fn the_listing_renders_the_derived_states_in_the_registry_order() {
    let (_home, features) = features();
    write_session(&features, "sess");
    let registry = r#"{"plans": [
      {"id": 1, "title": "Draft", "takeoff": null, "closed": null,
       "versions": [{"v": 1, "file": "artifacts/plan_01.html", "created_at": "2026-07-20T00:00:00+00:00",
                     "trigger": "initial", "base": null, "note": null}]},
      {"id": 2, "title": "Approved", "closed": null,
       "takeoff": {"v": 1, "at": "2026-07-20T01:00:00+00:00"},
       "versions": [{"v": 1, "file": "artifacts/plan_02.html", "created_at": "2026-07-20T00:00:00+00:00",
                     "trigger": "initial", "base": null, "note": null}]}
    ]}"#;
    fs::write(features.store.session_dir("sess").join(PLANS_FILE), registry).expect("the registry is written");
    let (status, body) = get(&features, "/api/sessions/sess/plans").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
      String::from_utf8(body).expect("the body is UTF-8"),
      r#"{"plans":[{"id":1,"title":"Draft","versions":[{"v":1,"file":"artifacts/plan_01.html","created_at":"2026-07-20T00:00:00+00:00","trigger":"initial","base":null,"note":null}],"takeoff":null,"closed":null,"state":"awaiting approval"},{"id":2,"title":"Approved","versions":[{"v":1,"file":"artifacts/plan_02.html","created_at":"2026-07-20T00:00:00+00:00","trigger":"initial","base":null,"note":null}],"takeoff":{"v":1,"at":"2026-07-20T01:00:00+00:00"},"closed":null,"state":"approved"}],"errors":[]}"#
    );
  }

  #[test]
  fn the_route_claims_the_plans_path_of_one_session() {
    assert!(matches("/api/sessions/sess/plans"));
    assert!(!matches("/api/sessions//plans"));
    assert!(!matches("/api/sessions/sess/plans/extra"));
    assert!(!matches("/api/sessions/sess"));
  }
}
