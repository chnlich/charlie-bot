//! The shared HTTP answers for task-domain errors returned by route helpers.

use axum::http::StatusCode;
use axum::response::Response;
use charliebot_compat::responses;
use charliebot_store::StoreError;

/// Translate one task-domain error into the FastAPI response used by structural routes.
pub fn task_http_error(error: StoreError) -> Response {
  let message = error.to_string();
  match error {
    StoreError::Failure(_) => responses::internal_error("RuntimeError", message),
    StoreError::Archived(_) => responses::json(StatusCode::CONFLICT, &serde_json::json!({ "detail": message })),
    StoreError::Invalid(_) => responses::json(StatusCode::BAD_REQUEST, &serde_json::json!({ "detail": message })),
    StoreError::NotFound(_) => responses::json(StatusCode::NOT_FOUND, &serde_json::json!({ "detail": message })),
    StoreError::Forbidden(_) => responses::json(StatusCode::FORBIDDEN, &serde_json::json!({ "detail": message })),
    StoreError::Conflict(blockers) => responses::json(
      StatusCode::CONFLICT,
      &serde_json::json!({ "detail": { "message": message, "blockers": blockers } }),
    ),
  }
}

#[cfg(test)]
mod tests {
  use axum::body::to_bytes;
  use axum::http::StatusCode;
  use axum::response::Response;
  use charliebot_compat::responses::Failure;
  use charliebot_store::StoreError;
  use serde_json::{Value, json};

  use super::task_http_error;

  async fn assert_json(response: Response, status: StatusCode, expected: Value) {
    assert_eq!(response.status(), status);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&body).unwrap(), expected);
  }

  #[tokio::test]
  async fn archived_errors_use_their_sentence_as_the_detail() {
    assert_json(
      task_http_error(StoreError::Archived("session-a".into())),
      StatusCode::CONFLICT,
      json!({ "detail": "task session-a is archived" }),
    )
    .await;
  }

  #[tokio::test]
  async fn invalid_errors_use_bad_request() {
    assert_json(
      task_http_error(StoreError::Invalid("invalid task input".into())),
      StatusCode::BAD_REQUEST,
      json!({ "detail": "invalid task input" }),
    )
    .await;
  }

  #[tokio::test]
  async fn not_found_errors_use_not_found() {
    assert_json(
      task_http_error(StoreError::NotFound("task is missing".into())),
      StatusCode::NOT_FOUND,
      json!({ "detail": "task is missing" }),
    )
    .await;
  }

  #[tokio::test]
  async fn forbidden_errors_use_forbidden() {
    assert_json(
      task_http_error(StoreError::Forbidden("caller is not allowed".into())),
      StatusCode::FORBIDDEN,
      json!({ "detail": "caller is not allowed" }),
    )
    .await;
  }

  #[tokio::test]
  async fn conflicts_include_the_sentence_and_blockers() {
    assert_json(
      task_http_error(StoreError::Conflict(vec!["first blocker".into(), "second blocker".into()])),
      StatusCode::CONFLICT,
      json!({
        "detail": {
          "message": "first blocker; second blocker",
          "blockers": ["first blocker", "second blocker"]
        }
      }),
    )
    .await;
  }

  #[tokio::test]
  async fn an_empty_conflict_keeps_an_empty_blocker_list() {
    assert_json(
      task_http_error(StoreError::Conflict(Vec::new())),
      StatusCode::CONFLICT,
      json!({ "detail": { "message": "", "blockers": [] } }),
    )
    .await;
  }

  #[tokio::test]
  async fn failures_use_the_unhandled_exception_response() {
    let response = task_http_error(StoreError::Failure("synthetic store failure".into()));
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let failure = response.extensions().get::<Failure>().unwrap();
    assert_eq!(failure.kind, "RuntimeError");
    assert_eq!(failure.detail, "synthetic store failure");
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(body.as_ref(), b"Internal Server Error");
  }
}
