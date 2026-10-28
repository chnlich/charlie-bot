//! The access gate in front of the web search MCP endpoint: the router registers under the server's gate, so a
//! request without the access key or a valid run token gets the JSON 401.

use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode, header};
use charliebot_compat::mimetypes::MimeTypes;
use charliebot_core::hot_reload::HotReload;
use charliebot_registry::FeatureContext;
use charliebot_store::Store;
use tower::ServiceExt;

use crate::app;
use crate::buildinfo::BuildInfo;
use crate::state::AppState;

use super::registry;

/// The server over a home whose access key is `synthetic-access-key`.
fn server(
  home: &std::path::Path,
) -> tower::util::BoxCloneSyncService<Request<Body>, axum::response::Response, std::convert::Infallible> {
  let registry = Arc::new(registry().expect("the listed packages register"));
  let config = registry.load_config(home).expect("the test config loads");
  let store = Arc::new(
    Store::new(config, registry.metadata_slots(), charliebot_registry::store_hooks::store_hooks(Arc::clone(&registry)))
      .expect("the test store"),
  );
  let config_registry = Arc::clone(&registry);
  let config_home = home.to_path_buf();
  let credentials_home = home.to_path_buf();
  let features = Arc::new(FeatureContext {
    config: Arc::new(HotReload::new(
      home.join("config.yaml"),
      "test_config_reload_failed",
      Box::new(move || config_registry.load_config(&config_home)),
    )),
    credentials: Arc::new(HotReload::new(
      home.join("credentials.yaml"),
      "test_credentials_reload_failed",
      Box::new(move || charliebot_core::credentials::load_credentials(&credentials_home)),
    )),
    store,
    registry,
    startup: Arc::new(Default::default()),
  });
  let state = Arc::new(AppState {
    features,
    build_info: BuildInfo { sha: "synthetic".to_string(), started_at: "synthetic".to_string() },
    static_mounts: Vec::new(),
    mime_types: MimeTypes::builtin(),
  });
  app::build(state)
}

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
  let service = server(home.path());

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
