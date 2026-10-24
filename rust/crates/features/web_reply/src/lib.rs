//! `src/features/web_reply`: the internal routes behind `charliebot web reply`, `charliebot web dialog` and
//! `charliebot web status`, the turn contribution that carries the reply rules into a web session's manager runs,
//! runs the round-end audit after a turn that answers a user, and renders the reply chat lines, and the web-reply
//! status field of each session.

mod api;
mod audit;
mod json_input;
mod plan_links;
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
use charliebot_store::metadata::UtcDateTime;
use serde_json::{Map, Value, json};
use turn_contribution::WebReplyTurnContribution;

/// `register` (`src/features/web_reply/__init__.py`): the `/api/internal` router, then the turn contribution, then the
/// web-reply status field of each session.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router(routes, matches)?;
  registry.turn_contribution("web_reply", Arc::new(WebReplyTurnContribution))?;
  registry.metadata_slot_with_validator(
    MetadataSlot {
      owner: "web_reply",
      on: MetadataFile::Session,
      after: None,
      fields: vec![SlotField { name: "web_status", default: serde_json::Value::Null }],
    },
    validate_session_fields,
  )
}

fn validate_session_fields(values: &mut Map<String, Value>) -> Option<String> {
  if let Some(status) = values.get_mut("web_status")
    && !normalize_web_status(status)
  {
    return Some("web_status must be a WebStatus or null".to_string());
  }
  None
}

fn normalize_web_status(value: &mut Value) -> bool {
  if value.is_null() {
    return true;
  }
  let Some(status) = value.as_object_mut() else { return false };
  status.retain(|name, _| ["goal", "now", "waiting", "updated_at"].contains(&name.as_str()));
  for name in ["goal", "now"] {
    if status.get(name).is_some_and(|field| {
      !field.is_null() && (!field.is_string() || field.as_str().is_some_and(|text| text.trim().is_empty()))
    }) {
      return false;
    }
    status.entry(name.to_string()).or_insert(Value::Null);
  }
  let waiting = status.entry("waiting".to_string()).or_insert_with(|| json!([]));
  let Some(items) = waiting.as_array_mut() else { return false };
  for item in items {
    let Some(item) = item.as_object_mut() else { return false };
    if item.keys().any(|name| !["text", "context", "link", "since", "since_exact"].contains(&name.as_str())) {
      return false;
    }
    if !item.get("text").and_then(Value::as_str).is_some_and(|text| !text.trim().is_empty()) {
      return false;
    }
    if item.get("link").is_some_and(|link| !link.is_null() && !link.is_string()) {
      return false;
    }
    match item.get("context") {
      None => {}
      Some(Value::Null) => {
        item.remove("context");
      }
      Some(Value::String(context)) if !context.trim().is_empty() => {}
      Some(_) => return false,
    }
    // `since` and `since_exact` are written by the server; an item stored before they existed carries neither, so a
    // null reads as absent, as the model's exclude-if-None fields do.
    match item.get("since") {
      None => {}
      Some(Value::Null) => {
        item.remove("since");
      }
      Some(Value::String(value)) if UtcDateTime::parse(value).is_ok() => {}
      Some(_) => return false,
    }
    match item.get("since_exact") {
      None => {}
      Some(Value::Null) => {
        item.remove("since_exact");
      }
      Some(Value::Bool(_)) => {}
      Some(_) => return false,
    }
    item.entry("link".to_string()).or_insert(Value::Null);
  }
  let updated_at =
    status.entry("updated_at".to_string()).or_insert_with(|| Value::String(UtcDateTime::now().to_python_string()));
  updated_at.as_str().is_some_and(|value| UtcDateTime::parse(value).is_ok())
}

/// The web reply routes, with the feature context as their state. The reply and dialog paths have one method each; the
/// status path answers POST and GET, and Starlette's 405 names the first route of the path, which is the POST route.
fn routes(features: &Arc<FeatureContext>) -> Router {
  Router::new()
    .route("/api/internal/web/reply", post_only(api::post_reply))
    .route("/api/internal/web/dialog", get_only(api::get_dialog))
    .route("/api/internal/web/status", refuse_other_methods(post(api::post_status).get(api::get_status), "POST"))
    .route("/api/internal/web/status/confirm", post_only(api::post_status_confirm))
    .route("/api/internal/web/status/unread", post_only(api::post_status_unread))
    .route("/api/internal/web/ask-check", post_only(api::post_ask_check))
    .with_state(Arc::clone(features))
}

/// Whether a path is one of the web reply routes, whatever the method. The server's trailing-slash redirect asks this
/// of the slash-toggled path.
fn matches(path: &str) -> bool {
  matches!(
    path,
    "/api/internal/web/reply"
      | "/api/internal/web/dialog"
      | "/api/internal/web/status"
      | "/api/internal/web/status/confirm"
      | "/api/internal/web/status/unread"
      | "/api/internal/web/ask-check"
  )
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
    assert!((routers[0].matches)("/api/internal/web/status/confirm"));
    assert!((routers[0].matches)("/api/internal/web/status/unread"));
    assert!((routers[0].matches)("/api/internal/web/ask-check"));
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

  #[tokio::test]
  async fn the_cold_read_routes_validate_record_and_gate_a_status_item() {
    let home = test_support::Home::new();
    let sid = home.manager("session", None).await;
    let item = json!({
      "text": "Approve the deploy",
      "context": "The release is ready. Options are deploy or wait; deploy is recommended.",
      "link": null,
    });
    let status_text = json!({"waiting": [item.clone()]}).to_string();
    let unread_body = json!({"session_id": sid, "text": status_text});

    let (status, body) = home.send(Method::POST, "/api/internal/web/status/unread", Some(unread_body.clone())).await;
    assert_eq!((status, body), (StatusCode::OK, json!({"unread": [item.clone()]})));

    let (status, body) = home
      .send(
        Method::POST,
        "/api/internal/web/ask-check",
        Some(json!({"session_id": sid, "item": item.clone(), "backend": "haiku", "answer": "Deploy."})),
      )
      .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["item_sha256"].as_str().unwrap().len(), 64);

    let (status, body) = home.send(Method::POST, "/api/internal/web/status/unread", Some(unread_body)).await;
    assert_eq!((status, body), (StatusCode::OK, json!({"unread": []})));

    let (status, body) =
      home.send(Method::POST, "/api/internal/web/status", Some(json!({"session_id": sid, "text": status_text}))).await;
    assert_eq!(status, StatusCode::OK);
    let stored_item = &body["waiting"][0];
    assert_eq!(stored_item["text"], item["text"]);
    assert_eq!(stored_item["context"], item["context"]);
    assert_eq!(stored_item["link"], item["link"]);
    assert!(stored_item.get("since").is_some(), "a stored item carries its start time");
    assert_eq!(stored_item["since_exact"], json!(true));
    assert_eq!(body["now_chars"], json!(0));
    assert_eq!(body["now_over_budget"], json!(false));

    let (status, body) = home
      .send(
        Method::POST,
        "/api/internal/web/ask-check",
        Some(json!({"session_id": sid, "item": 5, "backend": "haiku", "answer": "Deploy."})),
      )
      .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["detail"][0]["type"], "dict_type");
    assert_eq!(body["detail"][0]["loc"], json!(["body", "item"]));

    let ask_checks = home.events(&sid).into_iter().filter(|event| event["type"] == "web_ask_check").collect::<Vec<_>>();
    assert_eq!(ask_checks.len(), 1);
    assert_eq!(ask_checks[0]["backend"], "haiku");
    assert_eq!(ask_checks[0]["answer"], "Deploy.");
  }
}
