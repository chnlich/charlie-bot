//! The internal web routes (`src/features/web_reply/api.py`) behind `charliebot web reply`, `charliebot web dialog` and
//! `charliebot web status`.
//!
//! FastAPI validates the body or the query before the handler runs, so a malformed request answers 422 whatever the
//! session is. A refusal of the handler answers its status with `{"detail": ...}`.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, header};
use axum::response::Response;
use charliebot_compat::responses;
use charliebot_compat::validation::{self, FieldError, Kind, Loc, Source};
use charliebot_registry::FeatureContext;
use http::StatusCode;
use serde_json::{Map, Value, json};

use crate::reply::{self, WebReplyError};
use crate::status;

/// `POST /api/internal/web/reply`: records the body's text as the session's web reply.
pub async fn post_reply(State(features): State<Arc<FeatureContext>>, headers: HeaderMap, body: Bytes) -> Response {
  let request = match SessionText::parse(&headers, &body) {
    Ok(request) => request,
    Err(errors) => return validation::response(&errors),
  };
  answer(reply::post_web_reply(&features, &request.session_id, &request.text).await)
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
pub async fn post_status(State(features): State<Arc<FeatureContext>>, headers: HeaderMap, body: Bytes) -> Response {
  let request = match SessionText::parse(&headers, &body) {
    Ok(request) => request,
    Err(errors) => return validation::response(&errors),
  };
  answer(status::set_status(&features, &request.session_id, &request.text).await)
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

/// `WebReplyRequest` and `WebStatusRequest`: the calling session's id and a text, both required strings. Unknown keys
/// are refused, as `extra="forbid"` refuses them.
struct SessionText {
  session_id: String,
  text: String,
}

impl SessionText {
  /// FastAPI reads the body, then pydantic validates the model: a missing or mistyped field lists in declaration
  /// order, and the extra keys follow.
  fn parse(headers: &HeaderMap, body: &[u8]) -> Result<Self, Vec<FieldError>> {
    let value = read_body(headers, body)?;
    let fields = match &value {
      Value::Object(fields) => fields,
      Value::Null => return Err(vec![body_error(None, Value::Null, Kind::Missing)]),
      other => return Err(vec![body_error(None, other.clone(), Kind::ModelAttributesType)]),
    };
    let mut errors = Vec::new();
    let session_id = string_field(fields, &value, "session_id", &mut errors);
    let text = string_field(fields, &value, "text", &mut errors);
    for key in fields.keys().filter(|key| !matches!(key.as_str(), "session_id" | "text")) {
      errors.push(body_error(Some(key), fields[key].clone(), Kind::ExtraForbidden));
    }
    match (session_id, text) {
      (Some(session_id), Some(text)) if errors.is_empty() => Ok(Self { session_id, text }),
      _ => Err(errors),
    }
  }
}

/// A required string field: its text, or a `missing` or `string_type` error.
fn string_field(fields: &Map<String, Value>, body: &Value, name: &str, errors: &mut Vec<FieldError>) -> Option<String> {
  match fields.get(name) {
    Some(Value::String(text)) => Some(text.clone()),
    Some(input) => {
      errors.push(body_error(Some(name), input.clone(), Kind::StringType));
      None
    }
    None => {
      errors.push(body_error(Some(name), body.clone(), Kind::Missing));
      None
    }
  }
}

/// The body as FastAPI hands it to the model: the parsed JSON when the request is JSON or names no type, else the raw
/// text. An empty body is `null`, which the model reports as missing.
fn read_body(headers: &HeaderMap, body: &[u8]) -> Result<Value, Vec<FieldError>> {
  if body.is_empty() {
    return Ok(Value::Null);
  }
  let json_body = match headers.get(header::CONTENT_TYPE) {
    None => true,
    Some(content_type) => is_json_media_type(content_type.to_str().unwrap_or_default()),
  };
  if !json_body {
    return Ok(Value::String(String::from_utf8_lossy(body).into_owned()));
  }
  serde_json::from_slice(body).map_err(|error| {
    // The position is serde's own (line and column), counted in characters; the message is serde's, not CPython's.
    let mut loc = body_loc(None);
    loc.push(Loc::Index(char_position(body, &error)));
    vec![FieldError { loc, input: json!({}), kind: Kind::JsonInvalid { error: error.to_string() } }]
  })
}

/// The character offset of a JSON error's position in `body`.
fn char_position(body: &[u8], error: &serde_json::Error) -> usize {
  let line_start: usize =
    body.split_inclusive(|byte| *byte == b'\n').take(error.line().saturating_sub(1)).map(<[u8]>::len).sum();
  let end = (line_start + error.column().saturating_sub(1)).min(body.len());
  String::from_utf8_lossy(&body[..end]).chars().count()
}

/// `application/json` or a `+json` subtype, as FastAPI reads the request's content type.
fn is_json_media_type(content_type: &str) -> bool {
  let media = content_type.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
  match media.split_once('/') {
    Some(("application", subtype)) => subtype == "json" || subtype.ends_with("+json"),
    _ => false,
  }
}

/// The location of a body field, or of the body itself when `field` is `None`.
fn body_loc(field: Option<&str>) -> Vec<Loc> {
  let mut loc = vec![Loc::Field("body".to_string())];
  loc.extend(field.map(|name| Loc::Field(name.to_string())));
  loc
}

fn body_error(field: Option<&str>, input: Value, kind: Kind) -> FieldError {
  FieldError { loc: body_loc(field), input, kind }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn json_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    headers
  }

  #[test]
  fn a_session_text_body_reads_both_fields_and_refuses_extra_keys() {
    let body = br#"{"session_id": "s1", "text": "hi"}"#;
    let parsed = SessionText::parse(&json_headers(), body).expect("a complete body parses");
    assert_eq!((parsed.session_id.as_str(), parsed.text.as_str()), ("s1", "hi"));

    let errors = SessionText::parse(&json_headers(), br#"{"session_id": "s1", "text": "hi", "extra": 1}"#)
      .err()
      .expect("an extra key is refused");
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].loc, body_loc(Some("extra")));
  }

  #[test]
  fn a_missing_or_mistyped_field_lists_in_declaration_order() {
    let errors = SessionText::parse(&json_headers(), br#"{"text": 5}"#).err().expect("both fields are refused");
    let locs: Vec<&Vec<Loc>> = errors.iter().map(|error| &error.loc).collect();
    assert_eq!(locs, vec![&body_loc(Some("session_id")), &body_loc(Some("text"))]);
    assert!(matches!(errors[0].kind, Kind::Missing));
    assert!(matches!(errors[1].kind, Kind::StringType));
  }

  #[test]
  fn an_out_of_range_number_stays_a_field_type_error() {
    let errors = SessionText::parse(&json_headers(), br#"{"session_id": 123456789012345678901234567890, "text": "x"}"#)
      .err()
      .expect("a number is not a session id");
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].loc, body_loc(Some("session_id")));
    assert!(matches!(errors[0].kind, Kind::StringType));
  }
}
