//! The internal web routes (`src/features/web_reply/api.py`) behind `charliebot web reply`, `charliebot web dialog` and
//! `charliebot web status`.
//!
//! FastAPI validates the body or the query before the handler runs, so a malformed request answers 422 whatever the
//! session is. A refusal of the handler answers its status with `{"detail": ...}`.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::Request;
use axum::response::Response;
use charliebot_compat::fastapi_body::{self, BodyModel, Field, FieldType, RequestBody};
use charliebot_compat::responses;
use charliebot_compat::validation::{self, FieldError, Kind, Loc, Source};
use charliebot_registry::FeatureContext;
use http::StatusCode;
use serde_json::{Map, Value, json};

use crate::reply::{self, WebReplyError};
use crate::status;

/// `POST /api/internal/web/reply`: records the body's text as the session's web reply.
pub async fn post_reply(State(features): State<Arc<FeatureContext>>, request: Request<Body>) -> Response {
  let (parts, stream) = request.into_parts();
  let body = match fastapi_body::read(&parts.headers, stream).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  let model = match SessionText::validate(&body) {
    Ok(model) => model,
    Err(response) => return *response,
  };
  answer(reply::post_web_reply(&features, &model.session_id, &model.text).await)
}

/// `GET /api/internal/web/dialog?session_id=`: the session's For-you layer, in log order.
pub async fn get_dialog(
  State(features): State<Arc<FeatureContext>>,
  Query(query): Query<HashMap<String, String>>,
) -> Response {
  let session_id = match session_id_param(&query) {
    Ok(session_id) => session_id,
    Err(error) => return validation::response(&[*error]),
  };
  answer(dialog(&features, &session_id).await)
}

/// `POST /api/internal/web/status`: stores the items the body's JSON text names and answers the stored status.
pub async fn post_status(State(features): State<Arc<FeatureContext>>, request: Request<Body>) -> Response {
  let (parts, stream) = request.into_parts();
  let body = match fastapi_body::read(&parts.headers, stream).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  let model = match SessionText::validate(&body) {
    Ok(model) => model,
    Err(response) => return *response,
  };
  answer(status::set_status(&features, &model.session_id, &model.text).await)
}

/// `GET /api/internal/web/status?session_id=`: the session's status and its waiting descendants.
pub async fn get_status(
  State(features): State<Arc<FeatureContext>>,
  Query(query): Query<HashMap<String, String>>,
) -> Response {
  let session_id = match session_id_param(&query) {
    Ok(session_id) => session_id,
    Err(error) => return validation::response(&[*error]),
  };
  answer(status::read_status(&features, &session_id).await)
}

/// The For-you layer's answer: `{session_id, entries}`, after the session check every web route makes.
async fn dialog(features: &FeatureContext, session_id: &str) -> Result<Value, WebReplyError> {
  reply::require_web_session(features, session_id).await?;
  let events = reply::load_events(features, session_id).await?;
  let entries = reply::for_you_entries(&features.store, &events)?;
  Ok(json!({"session_id": session_id, "entries": entries}))
}

/// A handler's answer: the body with 200, a refusal with its status and detail, or the unhandled-exception 500.
fn answer(result: Result<Value, WebReplyError>) -> Response {
  match result {
    Ok(body) => responses::json(StatusCode::OK, &body),
    Err(WebReplyError::Refused { status: code, detail }) => responses::json(code, &json!({"detail": detail})),
    Err(WebReplyError::Failed(error)) => responses::internal_error("RuntimeError", format!("{error:#}")),
  }
}

/// The required `session_id` query parameter.
fn session_id_param(query: &HashMap<String, String>) -> Result<String, Box<FieldError>> {
  match query.get("session_id") {
    Some(session_id) => Ok(session_id.clone()),
    None => Err(Box::new(validation::missing(Source::Query, "session_id"))),
  }
}

/// The fields of `WebReplyRequest` and `WebStatusRequest`: the calling session's id and a text, both required strings.
const SESSION_TEXT_FIELDS: &[Field] =
  &[Field::required("session_id", FieldType::String), Field::required("text", FieldType::String)];
const SESSION_TEXT_BODY: BodyModel = BodyModel::required(SESSION_TEXT_FIELDS);

/// The validated `WebReplyRequest` or `WebStatusRequest`, whose model forbids extra keys.
struct SessionText {
  session_id: String,
  text: String,
}

impl SessionText {
  /// Validates the body against `SESSION_TEXT_BODY` and refuses the keys the model does not declare.
  fn validate(body: &RequestBody) -> Result<Self, Box<Response>> {
    let validated = body.validate(&SESSION_TEXT_BODY);
    if let RequestBody::Json(Value::Object(raw)) = body {
      let extras: Vec<FieldError> = raw
        .iter()
        .filter(|(key, _)| !matches!(key.as_str(), "session_id" | "text"))
        .map(|(key, value)| field_error(key, value.clone(), Kind::ExtraForbidden))
        .collect();
      if !extras.is_empty() {
        let mut errors = match &validated {
          Ok(_) => Vec::new(),
          Err(_) => session_text_field_errors(raw),
        };
        errors.extend(extras);
        return Err(Box::new(validation::response(&errors)));
      }
    }
    let Some(Value::Object(mut fields)) = validated? else {
      unreachable!("a required model validates a body to its fields")
    };
    Ok(Self { session_id: text_field(&mut fields, "session_id"), text: text_field(&mut fields, "text") })
  }
}

/// A string field of a validated model.
fn text_field(fields: &mut Map<String, Value>, name: &str) -> String {
  match fields.remove(name) {
    Some(Value::String(text)) => text,
    _ => unreachable!("a validated {name} is a string"),
  }
}

/// A validation error at `body.<field>`.
fn field_error(field: &str, input: Value, kind: Kind) -> FieldError {
  FieldError { loc: vec![Loc::Field("body".to_string()), Loc::Field(field.to_string())], input, kind }
}

/// The declared-field errors for an object body, used to retain them alongside forbidden extras.
fn session_text_field_errors(fields: &Map<String, Value>) -> Vec<FieldError> {
  let mut errors = Vec::new();
  for name in ["session_id", "text"] {
    match fields.get(name) {
      None => errors.push(field_error(name, Value::Null, Kind::Missing)),
      Some(Value::String(_)) => {}
      Some(value) => errors.push(field_error(name, value.clone(), Kind::StringType)),
    }
  }
  errors
}

#[cfg(test)]
mod tests {
  use axum::http::Method;

  use super::*;
  use crate::test_support::Home;

  #[tokio::test]
  async fn an_extra_key_answers_422_extra_forbidden_once_the_declared_fields_validate() {
    let home = Home::new();
    let sid = home.manager("session", None).await;
    let body = json!({"session_id": sid, "text": "x", "extra": 1});

    for path in ["/api/internal/web/reply", "/api/internal/web/status"] {
      let (status, detail) = home.send(Method::POST, path, Some(body.clone())).await;
      assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{path}");
      assert_eq!(detail["detail"][0]["type"], "extra_forbidden", "{path}");
      assert_eq!(detail["detail"][0]["loc"], json!(["body", "extra"]), "{path}");
    }
  }

  #[tokio::test]
  async fn a_missing_and_a_mistyped_field_list_in_declaration_order() {
    let home = Home::new();

    let (status, detail) = home.send(Method::POST, "/api/internal/web/reply", Some(json!({"text": 5}))).await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(detail["detail"][0]["type"], "missing");
    assert_eq!(detail["detail"][0]["loc"], json!(["body", "session_id"]));
    assert_eq!(detail["detail"][1]["type"], "string_type");
    assert_eq!(detail["detail"][1]["loc"], json!(["body", "text"]));
  }

  #[tokio::test]
  async fn extra_keys_are_reported_alongside_missing_and_mistyped_fields() {
    let home = Home::new();
    let body = json!({"text": 5, "extra": 1});

    for path in ["/api/internal/web/reply", "/api/internal/web/status"] {
      let (status, detail) = home.send(Method::POST, path, Some(body.clone())).await;
      assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{path}");
      assert_eq!(
        detail["detail"].as_array().unwrap().iter().map(|error| error["type"].as_str().unwrap()).collect::<Vec<_>>(),
        ["missing", "string_type", "extra_forbidden"],
        "{path}"
      );
      assert_eq!(
        detail["detail"].as_array().unwrap().iter().map(|error| error["loc"].clone()).collect::<Vec<_>>(),
        [json!(["body", "session_id"]), json!(["body", "text"]), json!(["body", "extra"])],
        "{path}"
      );
    }
  }
}
