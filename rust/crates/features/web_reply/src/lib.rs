//! `web/features/web_reply`: the internal routes behind `charliebot web reply`, `charliebot web dialog` and
//! `charliebot web status`, the turn contribution that carries the reply rules into a web session's manager runs,
//! runs the round-end audit after a turn that answers a user, and renders the reply chat lines, and the web-reply
//! status field of each session.

mod api;
mod audit;
pub mod cli;
mod json_input;
pub mod metadata;
mod plan_links;
mod reply;
pub mod status;
mod turn_contribution;

#[cfg(test)]
mod test_support;

use std::sync::Arc;

use axum::Router;
use axum::routing::post;
use charliebot_compat::routing::{get_only, post_only, refuse_other_methods};
use charliebot_core::event_types;
use charliebot_core::metadata_slot::{MetadataFile, MetadataSlot, SlotField};
use charliebot_registry::{FeatureContext, Registry, RegistryError};
use charliebot_store::metadata::UtcDateTime;
use serde_json::{Map, Value, json};
use turn_contribution::WebReplyTurnContribution;

/// `register`: the `/api/internal` router, then the turn contribution, then the
/// web-reply status field of each session.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router(routes, matches)?;
  registry.open_waiting_items(status::open_waiting_rows)?;
  registry.turn_contribution(event_types::WEB_REPLY, Arc::new(WebReplyTurnContribution))?;
  registry.metadata_slot_with_validator(
    MetadataSlot {
      owner: event_types::WEB_REPLY,
      on: MetadataFile::Session,
      after: None,
      fields: vec![SlotField { name: event_types::WEB_STATUS, default: serde_json::Value::Null }],
    },
    validate_session_fields,
  )
}

fn validate_session_fields(values: &mut Map<String, Value>) -> Option<String> {
  if let Some(status) = values.get_mut(event_types::WEB_STATUS)
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
    if item.keys().any(|name| {
      ![
        "text",
        "context",
        "link",
        "since",
        "since_exact",
        "tier",
        "rule",
        "executor",
        "approval_rule_refs",
        "backend",
        "raw_tier_line",
      ]
      .contains(&name.as_str())
    }) {
      return false;
    }
    if !item.get("text").and_then(Value::as_str).is_some_and(|text| !text.trim().is_empty()) {
      return false;
    }
    if item.get("link").is_some_and(|link| !link.is_null() && !link.is_string()) {
      return false;
    }
    for name in ["tier", "rule", "executor", "backend", "raw_tier_line"] {
      if item.get(name).is_some_and(|field| !field.is_null() && !field.is_string()) {
        return false;
      }
    }
    if let Some(refs) = item.get("approval_rule_refs") {
      if refs.is_null() {
        item.remove("approval_rule_refs");
      } else if serde_json::from_value::<Vec<charliebot_core::approval_policy::ApprovalRuleRef>>(refs.clone()).is_err()
      {
        return false;
      }
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
    // null reads as absent, the same as a key the item leaves out.
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
  let updated_at = status.entry("updated_at".to_string()).or_insert_with(|| Value::String(UtcDateTime::now().to_str()));
  updated_at.as_str().is_some_and(|value| UtcDateTime::parse(value).is_ok())
}

/// The web reply routes, with the feature context as their state. The reply and dialog paths have one method each; the
/// status path answers POST and GET, and the 405 body names the first route of the path, which is the POST route.
fn routes(features: &Arc<FeatureContext>) -> Router {
  Router::new()
    .route("/api/internal/web/reply", post_only(api::post_reply))
    .route("/api/internal/web/reply/unread", post_only(api::post_reply_unread))
    .route("/api/internal/web/reply-check", post_only(api::post_reply_check))
    .route("/api/internal/web/dialog", get_only(api::get_dialog))
    .route("/api/internal/web/status", refuse_other_methods(post(api::post_status).get(api::get_status), "POST"))
    .route("/api/internal/web/approval-rules", get_only(api::get_approval_rules))
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
      | "/api/internal/web/reply/unread"
      | "/api/internal/web/reply-check"
      | "/api/internal/web/dialog"
      | "/api/internal/web/status"
      | "/api/internal/web/approval-rules"
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
    registry.register_package("features.web_reply", register).expect("the web reply package registers");

    let routers = registry.routers();
    assert_eq!(routers.len(), 1);
    assert!(routers[0].claims("/api/internal/web/reply"));
    assert!(routers[0].claims("/api/internal/web/reply/unread"));
    assert!(routers[0].claims("/api/internal/web/reply-check"));
    assert!(routers[0].claims("/api/internal/web/dialog"));
    assert!(routers[0].claims("/api/internal/web/status"));
    assert!(routers[0].claims("/api/internal/web/approval-rules"));
    assert!(routers[0].claims("/api/internal/web/status/confirm"));
    assert!(routers[0].claims("/api/internal/web/status/unread"));
    assert!(routers[0].claims("/api/internal/web/ask-check"));
    assert!(!routers[0].claims("/api/internal/web"));
    assert!(!routers[0].claims("/api/internal/web/status/extra"));
  }

  #[tokio::test]
  async fn the_routes_answer_the_readback_the_refusals_and_the_missing_query_as_recorded() {
    let home = test_support::Home::new();
    let sid = home.manager("session", None).await;
    let reply = "/api/internal/web/reply";

    let (status, body) = home.send(Method::POST, reply, Some(json!({"session_id": sid, "text": "the answer"}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"posted": true, "chars": 10, "over_budget": false}));

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
    let mut unread_item = item.clone();
    unread_item["rule"] = json!(null);
    assert_eq!((status, body), (StatusCode::OK, json!({"unread": [unread_item], "stale_assessments": []})));

    let (status, body) = home
      .send(
        Method::POST,
        "/api/internal/web/ask-check",
        Some(json!({
          "session_id": sid,
          "item": item.clone(),
          "backend": "haiku",
          "answer": "Deploy.\ntier: t0 rule: money executor: agent approval_rule_refs: []",
        })),
      )
      .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["item_sha256"].as_str().unwrap().len(), 64);

    let (status, body) = home.send(Method::POST, "/api/internal/web/status/unread", Some(unread_body)).await;
    assert_eq!((status, body), (StatusCode::OK, json!({"unread": [], "stale_assessments": []})));

    let (status, body) =
      home.send(Method::POST, "/api/internal/web/status", Some(json!({"session_id": sid, "text": status_text}))).await;
    assert_eq!(status, StatusCode::OK);
    let stored_item = &body["waiting"][0];
    assert_eq!(stored_item["text"], item["text"]);
    assert_eq!(stored_item["context"], item["context"]);
    assert_eq!(stored_item["link"], item["link"]);
    assert!(stored_item.get("since").is_some(), "a stored item carries its start time");
    assert_eq!(stored_item["since_exact"], json!(true));
    assert_eq!(stored_item["tier"], json!("t0"), "a stored item carries its cold read's tier");
    assert_eq!(stored_item["rule"], json!("money"));
    assert_eq!(stored_item["executor"], json!("agent"));
    assert_eq!(stored_item["approval_rule_refs"], json!([]));
    assert_eq!(stored_item["backend"], json!("haiku"));
    assert_eq!(stored_item["raw_tier_line"], json!("tier: t0 rule: money executor: agent approval_rule_refs: []"));
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
    assert_eq!(ask_checks[0]["answer"], "Deploy.\ntier: t0 rule: money executor: agent approval_rule_refs: []");
  }

  #[tokio::test]
  async fn the_cold_read_routes_validate_record_and_gate_a_web_reply() {
    let home = test_support::Home::new();
    let sid = home.manager("session", None).await;

    // 1. Plain reply posts at first submission
    let plain_text = "已确认修复完成，测试全部通过。";
    let (status, body) = home
      .send(Method::POST, "/api/internal/web/reply/unread", Some(json!({"session_id": sid, "text": plain_text})))
      .await;
    assert_eq!((status, body), (StatusCode::OK, json!({"unread": true, "code_findings": []})));

    let (status, body) =
      home.send(Method::POST, "/api/internal/web/reply", Some(json!({"session_id": sid, "text": plain_text}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["posted"], json!(true));

    // 2. Reply with bare commit hash is sent back with finding naming it, then posts on second submission
    let bare_hash_text = "已合并 a6799847a 修复状态折叠。";
    let (status, body) = home
      .send(Method::POST, "/api/internal/web/reply/unread", Some(json!({"session_id": sid, "text": bare_hash_text})))
      .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["unread"], json!(true));
    let code_findings = body["code_findings"].as_array().unwrap();
    assert_eq!(code_findings.len(), 1);
    assert!(code_findings[0].as_str().unwrap().contains("a6799847a"));

    // First attempt to post directly without prior check is refused with 422
    let (status, _) = home
      .send(Method::POST, "/api/internal/web/reply", Some(json!({"session_id": sid, "text": bare_hash_text})))
      .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Record check
    let (status, body) = home
      .send(
        Method::POST,
        "/api/internal/web/reply-check",
        Some(json!({
          "session_id": sid,
          "text": bare_hash_text,
          "backend": "code-assertions",
          "findings": code_findings,
        })),
      )
      .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["reply_sha256"].as_str().unwrap().len(), 64);

    // Second call to unread returns unread: false with remaining findings
    let (status, body) = home
      .send(Method::POST, "/api/internal/web/reply/unread", Some(json!({"session_id": sid, "text": bare_hash_text})))
      .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["unread"], json!(false));
    assert_eq!(body["remaining_findings"].as_array().unwrap().len(), 1);

    // Second submission posts and records remaining findings on the web_reply event
    let (status, body) = home
      .send(Method::POST, "/api/internal/web/reply", Some(json!({"session_id": sid, "text": bare_hash_text})))
      .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["posted"], json!(true));

    let events = home.events(&sid);
    let last_reply = events.iter().rfind(|e| e["type"] == "web_reply").unwrap();
    assert_eq!(last_reply["content"], json!(bare_hash_text));
    let reply_findings = last_reply["web_reply"]["findings"].as_array().unwrap();
    assert_eq!(reply_findings.len(), 1);
    assert!(reply_findings[0].as_str().unwrap().contains("a6799847a"));

    // 3. Reply with rule-defined term such as 读回 is flagged
    let rule_term_text = "把提交和读回要求交给测试。";
    let (status, body) = home
      .send(Method::POST, "/api/internal/web/reply/unread", Some(json!({"session_id": sid, "text": rule_term_text})))
      .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["unread"], json!(true));
    let code_findings = body["code_findings"].as_array().unwrap();
    assert_eq!(code_findings.len(), 1);
    assert!(code_findings[0].as_str().unwrap().contains("读回"));
  }
}
