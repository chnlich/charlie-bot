//! The internal routes behind `charliebot slack reply` and `charliebot slack ack`.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::Request;
use axum::response::Response;
use charliebot_chat_threads::thread_entry::{self, ThreadAdapter, ThreadDetail, ThreadReplyError};
use charliebot_compat::fastapi_body::{self, BodyModel, Field, FieldType};
use charliebot_compat::responses;
use charliebot_compat::routing::post_only;
use charliebot_registry::FeatureContext;
use http::StatusCode;
use serde_json::{Map, Value, json};

use crate::slack_listener;

/// The Slack routes mounted under `/api/internal`.
pub(crate) fn routes(features: &Arc<FeatureContext>) -> Router {
  Router::new()
    .route("/api/internal/slack/reply", post_only(reply))
    .route("/api/internal/slack/ack", post_only(ack))
    .with_state(Arc::clone(features))
}

/// Whether the path is one of the Slack internal routes.
pub(crate) fn matches(path: &str) -> bool {
  matches!(path, "/api/internal/slack/reply" | "/api/internal/slack/ack")
}

async fn reply(State(features): State<Arc<FeatureContext>>, request: Request<Body>) -> Response {
  let (parts, stream) = request.into_parts();
  let body = match fastapi_body::read(&parts.headers, stream).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  let _config = match features.config.get() {
    Ok(config) => config,
    Err(error) => return responses::internal_error("ConfigError", error.to_string()),
  };
  let _store = &features.store;
  let model = match body.validate(&SLACK_REPLY_BODY) {
    Ok(model) => model,
    Err(response) => return *response,
  };
  let Some(Value::Object(mut fields)) = model else {
    unreachable!("a required Slack reply body validates to its declared fields")
  };
  let session_id = take_text(&mut fields, "session_id");
  let text = take_text(&mut fields, "text");
  let adapter: Arc<dyn ThreadAdapter> = Arc::new(slack_listener::SlackThreadAdapter::lazy(Arc::clone(&features)));
  let result = execute_reply(&features, &adapter, &session_id, &text).await;
  answer(result)
}

async fn ack(State(features): State<Arc<FeatureContext>>, request: Request<Body>) -> Response {
  let (parts, stream) = request.into_parts();
  let body = match fastapi_body::read(&parts.headers, stream).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  let _config = match features.config.get() {
    Ok(config) => config,
    Err(error) => return responses::internal_error("ConfigError", error.to_string()),
  };
  let _store = &features.store;
  let model = match body.validate(&SLACK_ACK_BODY) {
    Ok(model) => model,
    Err(response) => return *response,
  };
  let Some(Value::Object(mut fields)) = model else {
    unreachable!("a required Slack ack body validates to its declared fields")
  };
  let session_id = take_text(&mut fields, "session_id");
  let message_ids = take_string_list(&mut fields, "message_ids");
  let adapter: Arc<dyn ThreadAdapter> = Arc::new(slack_listener::SlackThreadAdapter::lazy(Arc::clone(&features)));
  answer(execute_ack(&features, &adapter, &session_id, &message_ids).await)
}

async fn execute_reply(
  features: &Arc<FeatureContext>,
  adapter: &Arc<dyn ThreadAdapter>,
  session_id: &str,
  text: &str,
) -> Result<Value, ThreadReplyError> {
  thread_entry::assert_thread_fresh(adapter, features, session_id).await?;
  thread_entry::post_reply(adapter, features, session_id, text).await
}

async fn execute_ack(
  features: &Arc<FeatureContext>,
  adapter: &Arc<dyn ThreadAdapter>,
  session_id: &str,
  message_ids: &[String],
) -> Result<Value, ThreadReplyError> {
  thread_entry::ack_messages(adapter, features, session_id, message_ids).await
}

fn answer(result: Result<Value, ThreadReplyError>) -> Response {
  match result {
    Ok(body) => responses::json(StatusCode::OK, &body),
    Err(error) => {
      let detail = match error.detail {
        ThreadDetail::Text(text) => Value::String(text),
        ThreadDetail::Structured(value) => value,
      };
      responses::json(error.status, &json!({"detail": detail}))
    }
  }
}

fn take_text(fields: &mut Map<String, Value>, name: &str) -> String {
  match fields.remove(name) {
    Some(Value::String(text)) => text,
    _ => unreachable!("a validated {name} is a string"),
  }
}

fn take_string_list(fields: &mut Map<String, Value>, name: &str) -> Vec<String> {
  match fields.remove(name) {
    Some(Value::Array(items)) => items
      .into_iter()
      .map(|item| match item {
        Value::String(text) => text,
        _ => unreachable!("a validated {name} item is a string"),
      })
      .collect(),
    _ => unreachable!("a validated {name} is a list"),
  }
}

const SLACK_REPLY_FIELDS: &[Field] =
  &[Field::required("session_id", FieldType::String), Field::required("text", FieldType::String)];
const SLACK_REPLY_BODY: BodyModel = BodyModel::required(SLACK_REPLY_FIELDS).forbid_extra();

const SLACK_ACK_FIELDS: &[Field] =
  &[Field::required("session_id", FieldType::String), Field::required("message_ids", FieldType::StringList)];
const SLACK_ACK_BODY: BodyModel = BodyModel::required(SLACK_ACK_FIELDS).forbid_extra();

#[cfg(test)]
mod tests {
  use axum::body::to_bytes;
  use axum::http::{Method, Request as HttpRequest};
  use tower::ServiceExt;

  use super::*;
  use crate::slack_listener::SlackThreadAdapter;
  use crate::test_support::{FakeApi, Home};

  #[test]
  fn registers_both_internal_slack_paths() {
    assert!(matches("/api/internal/slack/reply"));
    assert!(matches("/api/internal/slack/ack"));
    assert!(!matches("/api/internal/slack"));
  }

  #[tokio::test]
  async fn both_routes_forbid_extra_keys_and_validate_declared_field_types() {
    let home = Home::new();
    for (path, body, field) in [
      ("/api/internal/slack/reply", json!({"session_id": "s", "text": "x", "extra": 1}), "extra"),
      ("/api/internal/slack/reply", json!({"session_id": 5, "text": "x"}), "session_id"),
      ("/api/internal/slack/ack", json!({"session_id": "s", "message_ids": [], "extra": 1}), "extra"),
      ("/api/internal/slack/ack", json!({"session_id": "s", "message_ids": "ts"}), "message_ids"),
    ] {
      let request = HttpRequest::builder()
        .method(Method::POST)
        .uri(path)
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
      let response = routes(&home.features).oneshot(request).await.unwrap();
      assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY, "{path} {body}");
      let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
      let body: Value = serde_json::from_slice(&bytes).unwrap();
      assert_eq!(body["detail"][0]["loc"], json!(["body", field]), "{path} {body}");
      assert_eq!(
        body["detail"][0]["type"],
        if field == "extra" {
          "extra_forbidden"
        } else if field == "message_ids" {
          "list_type"
        } else {
          "string_type"
        }
      );
    }
  }

  #[tokio::test]
  async fn gated_route_412_then_ack_then_reply_posts() {
    let home = Home::new();
    let session = home.thread_session("synthetic-slack-session", "1700000000.000100").await;
    home
      .features
      .store
      .update_slot_fields(
        &session.id,
        "slack",
        Map::from_iter([("slack_watermark_ts".to_string(), json!("1700000000.000050"))]),
      )
      .await
      .unwrap()
      .expect("the Slack session remains present");
    let api = FakeApi::start(&[]).await;
    api.set_thread_messages(vec![
      json!({"user": "U_ALLOWED", "ts": "1700000000.000050", "text": "summon"}),
      json!({"user": "U_ALLOWED", "ts": "1700000000.000110", "text": "first follow up"}),
      json!({"user": "U_ALLOWED", "bot_id": "B_TEST", "ts": "1700000000.000115", "text": "bot noise"}),
      json!({"user": "U_OTHER", "ts": "1700000000.000120", "text": "lurker"}),
      json!({"user": "U_ALLOWED", "ts": "1700000000.000130", "text": "second follow up"}),
    ]);
    let features = &home.features;
    let adapter: Arc<dyn ThreadAdapter> =
      Arc::new(SlackThreadAdapter::with_client(Arc::clone(features), Arc::clone(&api.client)));

    let stale = execute_reply(features, &adapter, &session.id, "the answer").await.unwrap_err();
    assert_eq!(stale.status, StatusCode::PRECONDITION_FAILED);
    let ThreadDetail::Structured(detail) = stale.detail else { panic!("stale thread has a structured refusal") };
    assert_eq!(detail["error"], "stale_thread");
    assert_eq!(
      detail["new_messages"],
      json!([
        {"ts": "1700000000.000110", "user": "U_ALLOWED", "text_preview": "first follow up"},
        {"ts": "1700000000.000130", "user": "U_ALLOWED", "text_preview": "second follow up"}
      ])
    );
    assert!(api.posts().is_empty());

    let acked =
      execute_ack(features, &adapter, &session.id, &["1700000000.000110".to_string(), "1700000000.000130".to_string()])
        .await
        .unwrap();
    assert_eq!(acked, json!({"acked": 2, "watermark_ts": "1700000000.000130"}));
    let reply = execute_reply(features, &adapter, &session.id, "the answer").await.unwrap();
    assert_eq!(reply["posted"], true);
    assert_eq!(api.posts(), [json!({"channel": "C_TEST", "text": "the answer", "thread_ts": "1700000000.000100"})]);
  }
}
