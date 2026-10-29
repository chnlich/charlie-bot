//! `GET /api/ext-usage` (`src/features/usage/ext_usage.py` `get_ext_usage`): the quota panel's snapshot.

use std::sync::Arc;

use axum::Router;
use axum::response::Response;
use charliebot_compat::{responses, routing};
use charliebot_registry::FeatureContext;
use charliebot_store::metadata::UtcDateTime;
use http::StatusCode;
use serde_json::{Value, json};

use crate::ext_usage::{Panel, panel};

/// The router of the `/api` prefix. The panel is process-wide, so the feature context is not needed.
pub fn routes(_features: &Arc<FeatureContext>) -> Router {
  router()
}

/// The paths the routes answer, for the trailing-slash redirect.
pub fn matches(path: &str) -> bool {
  path == "/api/ext-usage"
}

fn router() -> Router {
  Router::new().route("/api/ext-usage", routing::get_only(get_ext_usage))
}

async fn get_ext_usage() -> Response {
  responses::json(StatusCode::OK, &snapshot(panel(), UtcDateTime::now()))
}

/// The body `get_ext_usage` answers with: the error text while the panel holds no entry, else the annotated providers.
fn snapshot(panel: &Panel, now: UtcDateTime) -> Value {
  let providers = panel.annotated(now);
  if providers.is_empty() {
    return json!({"error": "Usage data not yet available"});
  }
  json!({"providers": providers})
}

#[cfg(test)]
mod tests {
  use axum::body::{Body, to_bytes};
  use http::{Method, Request, header};
  use tower::ServiceExt;

  use super::*;

  #[test]
  fn an_empty_panel_answers_the_not_yet_available_error() {
    let body = snapshot(&Panel::default(), UtcDateTime::now());
    assert_eq!(body, json!({"error": "Usage data not yet available"}));
  }

  #[tokio::test]
  async fn get_answers_the_error_text_while_the_shared_panel_is_empty() {
    let response = router()
      .oneshot(Request::builder().method(Method::GET).uri("/api/ext-usage").body(Body::empty()).unwrap())
      .await
      .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(bytes.as_ref(), br#"{"error":"Usage data not yet available"}"#);
  }

  #[tokio::test]
  async fn head_and_other_methods_answer_405_with_allow_get() {
    for method in [Method::HEAD, Method::POST] {
      let response = router()
        .oneshot(Request::builder().method(method).uri("/api/ext-usage").body(Body::empty()).unwrap())
        .await
        .unwrap();

      assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
      assert_eq!(response.headers()[header::ALLOW], "GET");
    }
  }
}
