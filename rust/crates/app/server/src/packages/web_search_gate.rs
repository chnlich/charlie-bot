//! The access gate in front of the web search MCP endpoint. The router registers under the server's gate, so a
//! request without the access key or a valid run token gets the JSON 401.

use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode, header};
use tower::ServiceExt;

use super::composed_app;

fn ping(authorization: Option<&str>) -> Request<Body> {
  let mut request =
    Request::builder().method(Method::POST).uri("/api/web-search/mcp").header(header::CONTENT_TYPE, "application/json");
  if let Some(authorization) = authorization {
    request = request.header(header::AUTHORIZATION, authorization);
  }
  request.body(Body::from(r#"{"jsonrpc": "2.0", "id": 1, "method": "ping"}"#)).expect("build the request")
}

#[tokio::test]
async fn the_mcp_route_refuses_a_request_without_a_valid_bearer() {
  let home = tempfile::tempdir().expect("a temporary home");
  std::fs::write(home.path().join("credentials.yaml"), "charliebot:\n  access_key: synthetic-access-key\n")
    .expect("the credentials file");
  let (service, _) = composed_app(home.path());

  for authorization in [None, Some("Bearer not-the-key")] {
    let response = service.clone().oneshot(ping(authorization)).await.expect("the gate answers");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{authorization:?}");
    let body = to_bytes(response.into_body(), usize::MAX).await.expect("read the refusal");
    assert_eq!(body.as_ref(), br#"{"detail": "Unauthorized"}"#);
  }

  let response = service.oneshot(ping(Some("Bearer synthetic-access-key"))).await.expect("the gate passes the key");
  assert_eq!(response.status(), StatusCode::OK);
  let body = to_bytes(response.into_body(), usize::MAX).await.expect("read the answer");
  let body: serde_json::Value = serde_json::from_slice(&body).expect("the answer is JSON");
  assert_eq!(body, serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": {}}));
}
