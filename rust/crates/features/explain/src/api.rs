//! The explain routes: the body, the session check, the request validation, and the
//! answers.
//!
//! The body is read before the session check runs, so malformed JSON answers 422 whatever the session is.
//! The session check then runs before the body model is validated, so a well-formed body for a missing session answers
//! 404.

use std::collections::HashMap;
use std::fmt::Display;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::Request;
use axum::response::Response;
use charliebot_compat::request_body::{self, BodyModel, Field, FieldType, RequestBody};
use charliebot_compat::responses;
use charliebot_compat::validation::{self, FieldError, Kind, Loc, Source};
use charliebot_registry::FeatureContext;
use http::StatusCode;
use serde_json::{Value, json};

use crate::explain;

/// `POST /api/sessions/{session_id}/explain`: 202 with the new pending entry, or 200 with the live one.
pub async fn post_explain(
  State(features): State<Arc<FeatureContext>>,
  Path(session_id): Path<String>,
  request: Request<Body>,
) -> Response {
  let (parts, stream) = request.into_parts();
  let body = match request_body::read(&parts.headers, stream).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  if let Some(response) = refusal(&features, &session_id).await {
    return response;
  }
  let config = match features.config.get() {
    Ok(cfg) => cfg,
    Err(error) => return failure(error),
  };
  let model = match ExplainRequest::validate(&body) {
    Ok(model) => model,
    Err(response) => return *response,
  };
  let option = match explain_backend(&features, &config, &model.backend) {
    Ok(option) => option,
    Err(response) => return *response,
  };
  match explain::request_explain(&features, &session_id, model.event_index, option, config).await {
    Ok((entry, created)) => {
      let status = if created { StatusCode::ACCEPTED } else { StatusCode::OK };
      responses::fast_json(status, &Value::Object(entry))
    }
    Err(error) => failure(error),
  }
}

/// The option one explain request runs on. A model id uses the same selection filters as a one-member pool.
fn explain_backend(
  features: &FeatureContext,
  cfg: &charliebot_core::config::CharlieBotConfig,
  backend_id: &str,
) -> Result<charliebot_core::config::BackendOption, Box<Response>> {
  cfg
    .backends
    .check_backend_id(backend_id)
    .map_err(|error| Box::new(responses::json(StatusCode::BAD_REQUEST, &json!({"detail": error}))))?;
  let selector =
    charliebot_agents::model_pool::Selector::from_features(features, cfg).map_err(|error| Box::new(failure(error)))?;
  match selector.select(backend_id, &[]).map_err(|error| Box::new(failure(error)))? {
    Some(option) => Ok(option),
    None => {
      let detail = selector.exhausted_message(backend_id).map_err(|error| Box::new(failure(error)))?;
      Err(Box::new(responses::json(StatusCode::BAD_REQUEST, &json!({"detail": detail}))))
    }
  }
}

/// `GET /api/sessions/{session_id}/explain?upto=`: the divider's entry, answer and error included.
pub async fn get_explain(
  State(features): State<Arc<FeatureContext>>,
  Path(session_id): Path<String>,
  Query(query): Query<HashMap<String, String>>,
) -> Response {
  if let Some(response) = refusal(&features, &session_id).await {
    return response;
  }
  let upto = match upto_param(&query) {
    Ok(upto) => upto,
    Err(error) => return validation::response(&[*error]),
  };
  match explain::get_explain_entry(&features, &session_id, upto).await {
    Ok(Some(entry)) => responses::fast_json(StatusCode::OK, &Value::Object(entry)),
    Ok(None) => {
      responses::json(StatusCode::NOT_FOUND, &json!({"detail": format!("no explain entry for event_index {upto}")}))
    }
    Err(error) => failure(error),
  }
}

/// `GET /api/sessions/{session_id}/explain/status`: every entry's `{state, backend, generated_at}`, bodies excluded.
pub async fn get_explain_status(
  State(features): State<Arc<FeatureContext>>,
  Path(session_id): Path<String>,
) -> Response {
  if let Some(response) = refusal(&features, &session_id).await {
    return response;
  }
  match explain::explain_status(&features, &session_id).await {
    Ok(summary) => responses::fast_json(StatusCode::OK, &Value::Object(summary)),
    Err(error) => failure(error),
  }
}

/// The `require_session` dependency: the response that refuses the request when the session is missing, else `None`.
async fn refusal(features: &FeatureContext, session_id: &str) -> Option<Response> {
  match features.store.require_session(session_id).await {
    Ok(_) => None,
    Err(error) if error.status_code() == Some(404) => {
      Some(responses::json(StatusCode::NOT_FOUND, &json!({"detail": error.to_string()})))
    }
    Err(error) => Some(failure(error)),
  }
}

/// An unexpected failure: the 500 `Internal Server Error`, with the full error chain in the request log.
fn failure(error: impl Display) -> Response {
  responses::internal_error("RuntimeError", format!("{error:#}"))
}

/// The required `upto` query parameter, read as an int.
fn upto_param(query: &HashMap<String, String>) -> Result<i64, Box<FieldError>> {
  match query.get("upto") {
    None => Err(Box::new(validation::missing(Source::Query, "upto"))),
    Some(text) => validation::parse_int(Source::Query, "upto", text, None, None),
  }
}

/// The fields of `ExplainRequest`: the divider's event index and the backend option id, both required. Unknown keys are
/// ignored.
const EXPLAIN_FIELDS: &[Field] =
  &[Field::required("event_index", FieldType::Integer), Field::required("backend", FieldType::String)];
const EXPLAIN_BODY: BodyModel = BodyModel::required(EXPLAIN_FIELDS);

/// The validated `ExplainRequest`.
struct ExplainRequest {
  event_index: i64,
  backend: String,
}

impl ExplainRequest {
  /// Validates the body against `EXPLAIN_BODY` and reads its two fields.
  fn validate(body: &RequestBody) -> Result<Self, Box<Response>> {
    let Some(Value::Object(mut fields)) = body.validate(&EXPLAIN_BODY)? else {
      unreachable!("a required model validates a body to its fields")
    };
    let index = fields.remove("event_index").expect("a validated body carries event_index");
    // The shared reader keeps integers above the signed 64-bit range as unsigned numbers; the divider index is an i64.
    let Some(event_index) = index.as_i64() else {
      return Err(Box::new(validation::response(&[field_error("event_index", index, Kind::IntParsing)])));
    };
    let backend = match fields.remove("backend") {
      Some(Value::String(backend)) => backend,
      _ => unreachable!("a validated backend is a string"),
    };
    Ok(Self { event_index, backend })
  }
}

/// A validation error at `body.<field>`.
fn field_error(field: &str, input: Value, kind: Kind) -> FieldError {
  FieldError { loc: vec![Loc::Field("body".to_string()), Loc::Field(field.to_string())], input, kind }
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::time::{Duration, SystemTime};

  use charliebot_agents::backend_availability;
  use charliebot_core::timefmt;
  use tower::ServiceExt;

  use super::*;
  use crate::test_support::{Home, OPTION_ID, Start, answer_script, delayed_answer_script};

  /// The response's status and its body as JSON; the 500 body is plain text, so it reads as a string.
  async fn send(features: &Arc<FeatureContext>, method: &str, uri: &str, body: Option<&str>) -> (StatusCode, Value) {
    let request = Request::builder()
      .method(method)
      .uri(uri)
      .header(http::header::CONTENT_TYPE, "application/json")
      .body(Body::from(body.unwrap_or_default().to_string()))
      .unwrap();
    let response = crate::routes(features).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value =
      serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()));
    (status, value)
  }

  fn post_body(event_index: i64) -> String {
    json!({"event_index": event_index, "backend": OPTION_ID, "unused": true}).to_string()
  }

  fn stale_time() -> String {
    timefmt::iso_from_system_time(SystemTime::now() - Duration::from_secs(601))
  }

  #[tokio::test]
  async fn a_missing_session_answers_404_on_every_route_once_the_body_reads() {
    let home = Home::new(vec![]);

    let (post, _) = send(&home.features, "POST", "/api/sessions/nope/explain", Some(&post_body(2))).await;
    let (get, _) = send(&home.features, "GET", "/api/sessions/nope/explain", None).await;
    let (status, body) = send(&home.features, "GET", "/api/sessions/nope/explain/status", None).await;

    assert_eq!((post, get, status), (StatusCode::NOT_FOUND, StatusCode::NOT_FOUND, StatusCode::NOT_FOUND));
    assert_eq!(body, json!({"detail": "Session not found"}));
  }

  #[tokio::test]
  async fn malformed_json_for_a_missing_session_answers_422_json_invalid_before_the_session_check() {
    let home = Home::new(vec![]);

    let (status, detail) = send(&home.features, "POST", "/api/sessions/nope/explain", Some("{not json")).await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(detail["detail"][0]["type"], "json_invalid");
    assert_eq!(detail["detail"][0]["loc"], json!(["body", 1]));
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_post_answers_202_with_the_pending_entry_and_200_with_the_live_one() {
    let home = Home::new(vec![Start::Script(delayed_answer_script(1, "Explained."))]);
    home.create_session("s1");
    home.append_round("s1", Some("The answer.")).await;

    let (created, entry) = send(&home.features, "POST", "/api/sessions/s1/explain", Some(&post_body(2))).await;
    let (live, again) = send(&home.features, "POST", "/api/sessions/s1/explain", Some(&post_body(2))).await;

    assert_eq!(created, StatusCode::ACCEPTED);
    assert_eq!(entry["state"], "pending");
    assert_eq!(entry["backend"], OPTION_ID);
    assert_eq!(entry["answer"], "");
    assert_eq!(entry["error"], Value::Null);
    assert_eq!(entry["generated_at"], Value::Null);
    assert_eq!(live, StatusCode::OK);
    assert_eq!(again, entry);
  }

  /// Acceptance ①: an explain request naming a pool id selects the pool's member and launches the one-shot on it. The
  /// entry records the member, not the pool.
  #[tokio::test]
  #[ignore = "integration"]
  async fn a_post_naming_a_pool_launches_on_the_member_the_pool_selects() {
    let home = Home::with_pool(vec![Start::Script(answer_script("Explained."))]);
    let cfg = home.features.config.get().unwrap();
    assert_eq!(
      explain_backend(&home.features, &cfg, OPTION_ID).unwrap().id,
      explain_backend(&home.features, &cfg, "auto").unwrap().id,
      "a model and its one-member pool select the same explain backend"
    );
    home.create_session("s1");
    home.append_round("s1", Some("The answer.")).await;
    let mut frames = crate::test_support::subscribe(&home.features.store, "s1");

    let body = json!({"event_index": 2, "backend": "auto"}).to_string();
    let (status, entry) = send(&home.features, "POST", "/api/sessions/s1/explain", Some(&body)).await;
    frames.recv().await.expect("the generation finishes");

    assert_eq!(status, StatusCode::ACCEPTED, "the explain route accepts a pool id");
    assert_eq!(entry["backend"], OPTION_ID, "the pending entry names the member the pool selected");
    assert_eq!(home.factory.builds(), 1, "the one-shot launches on the member");
    let (status, entry) = send(&home.features, "GET", "/api/sessions/s1/explain?upto=2", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(entry["state"], "ready");
    assert_eq!(entry["backend"], OPTION_ID);
    assert_eq!(entry["answer"], "Explained.");
  }

  #[tokio::test]
  async fn a_post_with_an_unknown_backend_answers_400_and_starts_nothing() {
    let home = Home::new(vec![]);
    home.create_session("s1");
    let body = json!({"event_index": 2, "backend": "no-such-backend"}).to_string();

    let (status, detail) = send(&home.features, "POST", "/api/sessions/s1/explain", Some(&body)).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
      detail,
      json!({
        "detail": "backend 'no-such-backend' is not a backends.options or backends.pools id; valid ids: ['scripted-explain']"
      })
    );
    assert_eq!(home.factory.builds(), 0);
  }

  #[tokio::test]
  async fn a_post_with_a_cooling_model_or_its_singleton_pool_returns_exhaustion_and_starts_nothing() {
    let home = Home::with_pool(vec![]);
    home.create_session("s1");
    home.append_round("s1", Some("The answer.")).await;
    backend_availability::backend_availability(&home.features)
      .unwrap()
      .record(OPTION_ID, None, backend_availability::REASON_QUOTA_ERROR, "synthetic quota error")
      .unwrap();
    for backend_id in [OPTION_ID, "auto"] {
      let body = json!({"event_index": 2, "backend": backend_id}).to_string();
      let (status, detail) = send(&home.features, "POST", "/api/sessions/s1/explain", Some(&body)).await;
      assert_eq!(status, StatusCode::BAD_REQUEST, "{backend_id}");
      assert!(
        detail["detail"]
          .as_str()
          .is_some_and(|message| { message.contains("scripted-explain") && message.contains("quota_error") }),
        "{backend_id}: {detail}"
      );
    }
    assert_eq!(home.factory.builds(), 0);
  }

  #[tokio::test]
  async fn a_post_whose_body_lacks_the_event_index_answers_422_with_the_field_location() {
    let home = Home::new(vec![]);
    home.create_session("s1");

    let (status, detail) =
      send(&home.features, "POST", "/api/sessions/s1/explain", Some(&json!({"backend": OPTION_ID}).to_string())).await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(detail["detail"][0]["type"], "missing");
    assert_eq!(detail["detail"][0]["loc"], json!(["body", "event_index"]));
    assert_eq!(detail["detail"][0]["msg"], "Field required");
  }

  #[tokio::test]
  async fn a_post_whose_body_is_not_json_answers_422_json_invalid() {
    let home = Home::new(vec![]);
    home.create_session("s1");

    let (status, detail) = send(&home.features, "POST", "/api/sessions/s1/explain", Some("{not json")).await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(detail["detail"][0]["type"], "json_invalid");
    assert_eq!(detail["detail"][0]["loc"], json!(["body", 1]));
  }

  #[tokio::test]
  async fn a_get_without_upto_answers_422_and_a_divider_without_an_entry_answers_404() {
    let home = Home::new(vec![]);
    home.create_session("s1");

    let (missing, detail) = send(&home.features, "GET", "/api/sessions/s1/explain", None).await;
    let (absent, body) = send(&home.features, "GET", "/api/sessions/s1/explain?upto=7", None).await;

    assert_eq!(missing, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
      detail,
      json!({"detail": [{"type": "missing", "loc": ["query", "upto"], "msg": "Field required", "input": null}]})
    );
    assert_eq!(absent, StatusCode::NOT_FOUND);
    assert_eq!(body, json!({"detail": "no explain entry for event_index 7"}));
  }

  #[tokio::test]
  #[ignore = "integration"]
  async fn a_get_answers_the_entry_with_its_answer_and_error_once_generated() {
    let home = Home::new(vec![Start::Script(answer_script("Explained."))]);
    home.create_session("s1");
    home.append_round("s1", Some("The answer.")).await;
    let mut frames = crate::test_support::subscribe(&home.features.store, "s1");
    send(&home.features, "POST", "/api/sessions/s1/explain", Some(&post_body(2))).await;
    frames.recv().await.expect("the generation finishes");

    let (status, entry) = send(&home.features, "GET", "/api/sessions/s1/explain?upto=2", None).await;
    let (_, summary) = send(&home.features, "GET", "/api/sessions/s1/explain/status", None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(entry["state"], "ready");
    assert_eq!(entry["answer"], "Explained.");
    assert_eq!(entry["error"], Value::Null);
    assert_eq!(summary["2"]["state"], "ready");
    assert_eq!(summary["2"]["backend"], OPTION_ID);
    assert!(summary["2"]["generated_at"].is_string());
    assert!(summary["2"].get("answer").is_none(), "the status body carries no answer");
  }

  #[tokio::test]
  async fn a_status_without_a_file_answers_an_empty_object() {
    let home = Home::new(vec![]);
    home.create_session("s1");

    let (status, body) = send(&home.features, "GET", "/api/sessions/s1/explain/status", None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({}));
  }

  #[tokio::test]
  async fn a_stale_pending_entry_is_reaped_by_the_get_route_and_answered_as_the_error() {
    let home = Home::new(vec![]);
    home.create_session("s1");
    let path = crate::explain::results_path(&home.features.store, "s1");
    let stale = json!({"4": {
      "state": "pending", "backend": OPTION_ID, "answer": "", "error": null,
      "requested_at": stale_time(), "generated_at": null,
    }});
    fs::write(&path, serde_json::to_string_pretty(&stale).unwrap()).unwrap();

    let (status, entry) = send(&home.features, "GET", "/api/sessions/s1/explain?upto=4", None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(entry["state"], "error");
    assert_eq!(entry["error"], "interrupted: server restarted");
    assert!(entry["generated_at"].is_string());
  }

  #[tokio::test]
  async fn an_unreadable_results_file_answers_500_on_every_route() {
    let home = Home::new(vec![]);
    home.create_session("s1");
    fs::write(crate::explain::results_path(&home.features.store, "s1"), "{not json").unwrap();

    let (post, _) = send(&home.features, "POST", "/api/sessions/s1/explain", Some(&post_body(2))).await;
    let (get, _) = send(&home.features, "GET", "/api/sessions/s1/explain?upto=2", None).await;
    let (status, _) = send(&home.features, "GET", "/api/sessions/s1/explain/status", None).await;

    assert_eq!(
      (post, get, status),
      (StatusCode::INTERNAL_SERVER_ERROR, StatusCode::INTERNAL_SERVER_ERROR, StatusCode::INTERNAL_SERVER_ERROR)
    );
  }
}
