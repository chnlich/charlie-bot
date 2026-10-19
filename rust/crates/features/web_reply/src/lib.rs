//! `src/features/web_reply`: the internal routes behind `charliebot web reply`, `charliebot web dialog` and
//! `charliebot web status`, the turn contribution that carries the reply rules into a web session's manager runs and
//! renders the reply chat lines, and the web-reply status field of each session. The round-end audit (`audit.py`) is
//! not ported yet; the package keeps its stub marker until the thread-core port lands it.

mod api;
mod reply;
mod status;
mod turn_contribution;

#[cfg(test)]
mod test_support;

use std::sync::Arc;

use axum::Router;
use axum::routing::post;
use charliebot_compat::routing::{get_only, post_only, refuse_other_methods};
use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot, SlotField};
use charliebot_registry::{FeatureContext, Registry, RegistryError};
use turn_contribution::WebReplyTurnContribution;

/// `register` (`src/features/web_reply/__init__.py`): the `/api/internal` router, then the turn contribution, then the
/// web-reply status field of each session.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router(routes, matches)?;
  registry.turn_contribution("web_reply", Arc::new(WebReplyTurnContribution))?;
  registry.metadata_slot(MetadataSlot {
    owner: "web_reply",
    on: MetadataFile::Session,
    after: None,
    fields: vec![SlotField { name: "web_status", default: serde_json::Value::Null }],
  })
}

/// The web reply routes, with the feature context as their state. The reply and dialog paths have one method each; the
/// status path answers POST and GET, and Starlette's 405 names the first route of the path, which is the POST route.
fn routes(features: &Arc<FeatureContext>) -> Router {
  Router::new()
    .route("/api/internal/web/reply", post_only(api::post_reply))
    .route("/api/internal/web/dialog", get_only(api::get_dialog))
    .route("/api/internal/web/status", refuse_other_methods(post(api::post_status).get(api::get_status), "POST"))
    .with_state(Arc::clone(features))
}

/// Whether a path is one of the web reply routes, whatever the method. The server's trailing-slash redirect asks this
/// of the slash-toggled path.
fn matches(path: &str) -> bool {
  matches!(path, "/api/internal/web/reply" | "/api/internal/web/dialog" | "/api/internal/web/status")
}

#[cfg(test)]
mod tests {
  use http::{Method, StatusCode};
  use serde_json::json;

  use super::*;

  #[test]
  fn registers_one_router_that_claims_the_web_reply_paths_only() {
    let mut registry = Registry::new();
    registry.register_package("src.features.web_reply", register).expect("the web reply package registers");

    let routers = registry.routers();
    assert_eq!(routers.len(), 1);
    assert!((routers[0].matches)("/api/internal/web/reply"));
    assert!((routers[0].matches)("/api/internal/web/dialog"));
    assert!((routers[0].matches)("/api/internal/web/status"));
    assert!(!(routers[0].matches)("/api/internal/web"));
    assert!(!(routers[0].matches)("/api/internal/web/status/extra"));
  }

  #[tokio::test]
  async fn the_routes_answer_the_readback_the_refusals_and_the_missing_query_as_fastapi_does() {
    let home = test_support::Home::new();
    let sid = home.manager("session", None).await;
    let reply = "/api/internal/web/reply";

    let (status, body) = home.send(Method::POST, reply, Some(json!({"session_id": sid, "text": "the answer"}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"posted": true, "chars": 10, "over_budget": false, "answers": null}));

    let (status, body) = home.send(Method::POST, reply, Some(json!({"session_id": "no-such", "text": "x"}))).await;
    assert_eq!((status, body), (StatusCode::NOT_FOUND, json!({"detail": "Session not found"})));

    let (status, body) = home.send(Method::POST, reply, Some(json!({"session_id": sid, "text": " "}))).await;
    assert_eq!((status, body), (StatusCode::UNPROCESSABLE_ENTITY, json!({"detail": "Reply text is empty"})));

    let (status, body) = home.send(Method::POST, reply, Some(json!({"session_id": 5, "text": "x"}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["detail"][0]["type"], json!("string_type"));
    assert_eq!(body["detail"][0]["loc"], json!(["body", "session_id"]));

    let (status, body) = home
      .send(Method::POST, "/api/internal/web/status", Some(json!({"session_id": sid, "text": r#"{"now": "Running"}"#})))
      .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["now"], json!("Running"));

    let (status, body) = home.send(Method::GET, "/api/internal/web/dialog", None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["detail"][0]["loc"], json!(["query", "session_id"]));

    let (status, body) = home.send(Method::GET, &format!("/api/internal/web/dialog?session_id={sid}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"session_id": sid, "entries": [{"role": "web_reply", "content": "the answer"}]}));
  }
}
