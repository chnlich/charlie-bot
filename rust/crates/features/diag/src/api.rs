//! The diag route (`src/features/diag/api.py`): `POST /api/diag/switch-events`, one validated telemetry point per
//! call, logged as a single `diag_switch` line. No session or state is consulted; the route answers `{"ok": true}`.

use std::borrow::Cow;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::Request;
use axum::response::Response;
use charliebot_compat::request_body::{self, BodyModel, Field, FieldType};
use charliebot_compat::responses;
use charliebot_compat::routing::post_only;
use charliebot_compat::validation::Bound;
use charliebot_registry::FeatureContext;
use http::StatusCode;
use serde_json::{Value, json};

/// The point logs at most this many code points of `error`, as Python's `error[:MAX_ERROR_CHARS]` keeps.
pub const MAX_ERROR_CHARS: usize = 300;

const PHASES: &[&str] = &["started", "completed", "superseded", "failed", "render_error"];

/// `SwitchEventRequest` in its declaration order, with `extra='forbid'`.
const SWITCH_EVENT_FIELDS: &[Field] = &[
  Field::required("phase", FieldType::Literal(PHASES)),
  Field::required("from_session", FieldType::NullableString),
  Field::required("to_session", FieldType::String),
  Field::required("generation", FieldType::Integer),
  Field::required("winner_generation", FieldType::NullableInteger),
  Field::optional("elapsed_ms", FieldType::NullableInteger).ge(Bound::Int(0)),
  Field::required("error", FieldType::NullableString),
  Field::required("client_ts", FieldType::String),
];

const SWITCH_EVENT_BODY: BodyModel = BodyModel::required(SWITCH_EVENT_FIELDS).forbid_extra();

/// The routes, with the feature context as their state. The path has one method.
pub fn routes(features: &Arc<FeatureContext>) -> Router {
  Router::new().route("/api/diag/switch-events", post_only(post_switch_event)).with_state(Arc::clone(features))
}

/// Whether a path is one of the diag routes, whatever the method. The server's trailing-slash redirect asks this of
/// the slash-toggled path.
pub fn matches(path: &str) -> bool {
  matches!(path, "/api/diag/switch-events")
}

/// `POST /api/diag/switch-events`: validate the point, log one `diag_switch` line, answer `{"ok": true}`.
async fn post_switch_event(State(_features): State<Arc<FeatureContext>>, request: Request<Body>) -> Response {
  let (parts, stream) = request.into_parts();
  let body = match request_body::read(&parts.headers, stream).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  let values = match body.validate(&SWITCH_EVENT_BODY) {
    Ok(Some(Value::Object(values))) => values,
    Ok(_) => unreachable!("a required model validates a body to its fields"),
    Err(response) => return *response,
  };
  let event = SwitchEvent::from_values(&values);
  log_switch_event(&event);
  responses::json(StatusCode::OK, &json!({"ok": true}))
}

/// The validated `SwitchEventRequest`.
pub struct SwitchEvent<'a> {
  pub phase: String,
  pub from_session: Option<String>,
  pub to_session: String,
  pub generation: i128,
  pub winner_generation: Option<i128>,
  pub elapsed_ms: Option<i128>,
  pub error: Option<&'a str>,
  pub client_ts: String,
}

impl<'a> SwitchEvent<'a> {
  /// Reads the validated fields in model declaration order and borrows `error` from the validated value.
  fn from_values(values: &'a serde_json::Map<String, Value>) -> Self {
    Self {
      phase: string(values, "phase"),
      from_session: optional_string(values, "from_session"),
      to_session: string(values, "to_session"),
      generation: integer(values, "generation").expect("a required integer validates to a value"),
      winner_generation: integer(values, "winner_generation"),
      elapsed_ms: integer(values, "elapsed_ms"),
      error: optional_str(values, "error"),
      client_ts: string(values, "client_ts"),
    }
  }
}

fn string(values: &serde_json::Map<String, Value>, name: &str) -> String {
  match values.get(name) {
    Some(Value::String(text)) => text.clone(),
    other => unreachable!("the validated body carries a string {name}, got {other:?}"),
  }
}

fn optional_string(values: &serde_json::Map<String, Value>, name: &str) -> Option<String> {
  match values.get(name) {
    Some(Value::String(text)) => Some(text.clone()),
    Some(Value::Null) | None => None,
    other => unreachable!("the validated body carries a string or null {name}, got {other:?}"),
  }
}

fn optional_str<'a>(values: &'a serde_json::Map<String, Value>, name: &str) -> Option<&'a str> {
  match values.get(name) {
    Some(Value::String(text)) => Some(text),
    Some(Value::Null) | None => None,
    other => unreachable!("the validated body carries a string or null {name}, got {other:?}"),
  }
}

/// An integer field of the validated body, `None` when the field is absent or null. `i128` holds the signed 64-bit
/// range and the positive values through the unsigned 64-bit maximum accepted by the FastAPI body reader.
fn integer(values: &serde_json::Map<String, Value>, name: &str) -> Option<i128> {
  match values.get(name) {
    None | Some(Value::Null) => None,
    Some(Value::Number(number)) => Some(
      number
        .as_i64()
        .map(i128::from)
        .or_else(|| number.as_u64().map(i128::from))
        .expect("the validated body carries a 64-bit integer"),
    ),
    other => unreachable!("the validated body carries an integer or null {name}, got {other:?}"),
  }
}

/// `error` cut to its first `MAX_ERROR_CHARS` code points, as Python's `error[:MAX_ERROR_CHARS]` keeps.
/// Like `src/features/diag/api.py:32-39`, borrow the validated string and inspect only the first character past the
/// limit before allocating the retained prefix.
pub fn cut_error<'a>(error: Option<&'a str>) -> Option<Cow<'a, str>> {
  error.map(|error| match error.char_indices().nth(MAX_ERROR_CHARS) {
    Some((end, _)) => Cow::Owned(error[..end].to_owned()),
    None => Cow::Borrowed(error),
  })
}

/// The `diag_switch` record: the validated fields in `model_dump()` order, with the cut error and an absent optional
/// logged as null. `error` is the cut error, which must outlive the record.
pub fn switch_record<'a>(event: &'a SwitchEvent<'_>, error: &'a str) -> Vec<(&'static str, LogValue<'a>)> {
  vec![
    ("phase", LogValue::Str(&event.phase)),
    ("from_session", LogValue::of(event.from_session.as_deref())),
    ("to_session", LogValue::Str(&event.to_session)),
    ("generation", LogValue::Int(event.generation)),
    ("winner_generation", LogValue::of_int(event.winner_generation)),
    ("elapsed_ms", LogValue::of_int(event.elapsed_ms)),
    (
      "error",
      match event.error {
        Some(_) => LogValue::of(Some(error)),
        None => LogValue::Null,
      },
    ),
    ("client_ts", LogValue::Str(&event.client_ts)),
  ]
}

/// One loggable value: a string or integer as itself, and a missing optional as `null`, which structlog's console
/// renderer spells `None` for and the JSON line spells `null`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LogValue<'a> {
  Str(&'a str),
  Int(i128),
  Null,
}

impl<'a> LogValue<'a> {
  fn of(value: Option<&'a str>) -> Self {
    match value {
      Some(text) => Self::Str(text),
      None => Self::Null,
    }
  }

  fn of_int(value: Option<i128>) -> Self {
    match value {
      Some(value) => Self::Int(value),
      None => Self::Null,
    }
  }
}

impl std::fmt::Display for LogValue<'_> {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Str(text) => formatter.write_str(text),
      Self::Int(value) => write!(formatter, "{value}"),
      Self::Null => formatter.write_str("null"),
    }
  }
}

/// The one `diag_switch` line, its fields in `model_dump()` order.
fn log_switch_event(event: &SwitchEvent<'_>) {
  let error = cut_error(event.error);
  let error = error.as_deref().unwrap_or("");
  let record = switch_record(event, error);
  let [phase, from_session, to_session, _, winner_generation, elapsed_ms, error, client_ts] = record[..] else {
    unreachable!("the record carries its eight fields");
  };
  tracing::info!(
    phase = %phase.1,
    from_session = %from_session.1,
    to_session = %to_session.1,
    generation = %event.generation,
    winner_generation = %winner_generation.1,
    elapsed_ms = %elapsed_ms.1,
    error = %error.1,
    client_ts = %client_ts.1,
    "diag_switch"
  );
}

#[cfg(test)]
mod tests {
  use charliebot_compat::request_body::RequestBody;
  use charliebot_compat::validation::{FieldError, Kind, Loc};
  use serde_json::json;

  use super::*;

  /// The base body of a valid `completed` point, every field set.
  fn valid_body() -> Value {
    json!({
      "phase": "completed",
      "from_session": "session-a",
      "to_session": "session-b",
      "generation": 3,
      "winner_generation": 2,
      "elapsed_ms": 1500,
      "error": null,
      "client_ts": "2026-02-13T10:00:00Z",
    })
  }

  fn check(body: Value) -> Result<Value, Vec<FieldError>> {
    RequestBody::Json(body).check(&SWITCH_EVENT_BODY).map(|value| value.expect("a required model validates an object"))
  }

  fn loc(error: &FieldError) -> Vec<String> {
    error
      .loc
      .iter()
      .map(|part| match part {
        Loc::Field(name) => name.clone(),
        Loc::Index(index) => index.to_string(),
      })
      .collect()
  }

  #[test]
  fn a_complete_point_validates_to_its_fields() {
    let values = check(valid_body()).expect("the body is valid");
    let names: Vec<&str> = values.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
      names,
      ["phase", "from_session", "to_session", "generation", "winner_generation", "elapsed_ms", "error", "client_ts"]
    );
    assert_eq!(values["phase"], "completed");
    assert_eq!(values["generation"], 3);
    assert_eq!(values["elapsed_ms"], 1500);
  }

  #[test]
  fn unsigned_values_above_i64_max_are_preserved() {
    let mut body = valid_body();
    body["generation"] = json!(i64::MAX as u64 + 1);
    body["winner_generation"] = json!(u64::MAX);
    body["elapsed_ms"] = json!(i64::MAX as u64 + 1);
    let values = RequestBody::Json(body)
      .validate(&SWITCH_EVENT_BODY)
      .expect("the body reader's integer range is valid")
      .expect("the body is present");
    let Value::Object(values) = values else { panic!("the body validates to its fields") };
    let event = SwitchEvent::from_values(&values);

    assert_eq!(event.generation, i64::MAX as i128 + 1);
    assert_eq!(event.winner_generation, Some(u64::MAX as i128));
    assert_eq!(event.elapsed_ms, Some(i64::MAX as i128 + 1));
    let record = switch_record(&event, "");
    assert_eq!(record[3].1, LogValue::Int(i64::MAX as i128 + 1));
    assert_eq!(record[4].1, LogValue::Int(u64::MAX as i128));
    assert_eq!(record[5].1, LogValue::Int(i64::MAX as i128 + 1));
  }

  #[test]
  fn validated_error_is_borrowed_and_short_cuts_reuse_the_input() {
    let mut body = valid_body();
    body["error"] = json!("short diagnostic");
    let values =
      RequestBody::Json(body).validate(&SWITCH_EVENT_BODY).expect("the body is valid").expect("the body is present");
    let Value::Object(values) = values else { panic!("the body validates to its fields") };
    let event = SwitchEvent::from_values(&values);
    let validated_error = values["error"].as_str().expect("the validated error is text");
    assert_eq!(event.error.unwrap().as_ptr(), validated_error.as_ptr());

    let short = cut_error(event.error).expect("the error is present");
    assert!(matches!(short, Cow::Borrowed(_)));
  }

  #[test]
  fn an_absent_elapsed_ms_validates_and_stays_absent() {
    let mut body = valid_body();
    body.as_object_mut().unwrap().remove("elapsed_ms");
    let values = check(body).expect("the body is valid");
    assert!(!values.as_object().unwrap().contains_key("elapsed_ms"));
  }

  #[test]
  fn an_unknown_phase_is_a_literal_error() {
    let mut body = valid_body();
    body["phase"] = json!("cancelled");
    let errors = check(body).expect_err("the phase is unknown");
    assert_eq!(loc(&errors[0]), ["body", "phase"]);
    assert!(matches!(errors[0].kind, Kind::Literal(_)));
  }

  #[test]
  fn a_negative_elapsed_ms_violates_its_ge_bound() {
    let mut body = valid_body();
    body["elapsed_ms"] = json!(-1);
    let errors = check(body).expect_err("the elapsed time is negative");
    assert_eq!(loc(&errors[0]), ["body", "elapsed_ms"]);
    assert_eq!(errors[0].kind, Kind::GreaterThanEqual(Bound::Int(0)));
  }

  #[test]
  fn an_extra_key_is_forbidden() {
    let mut body = valid_body();
    body["stale"] = json!(true);
    let errors = check(body).expect_err("the key is undeclared");
    assert_eq!(loc(&errors[0]), ["body", "stale"]);
    assert_eq!(errors[0].kind, Kind::ExtraForbidden);
  }

  #[test]
  fn a_missing_field_a_wrong_type_and_an_extra_key_report_in_pydantics_order() {
    let mut body = valid_body();
    body.as_object_mut().unwrap().remove("to_session");
    body["generation"] = json!("seven");
    body["stale"] = json!(true);
    let errors = check(body).expect_err("the body is invalid");
    assert_eq!(loc(&errors[0]), ["body", "to_session"]);
    assert_eq!(errors[0].kind, Kind::Missing);
    assert_eq!(loc(&errors[1]), ["body", "generation"]);
    assert_eq!(errors[1].kind, Kind::IntParsing);
    assert_eq!(loc(&errors[2]), ["body", "stale"]);
    assert_eq!(errors[2].kind, Kind::ExtraForbidden);
  }

  #[test]
  fn a_null_body_is_a_missing_body() {
    let errors = RequestBody::Json(Value::Null).check(&SWITCH_EVENT_BODY).expect_err("the body is null");
    assert_eq!(loc(&errors[0]), ["body"]);
    assert_eq!(errors[0].kind, Kind::Missing);
  }

  #[test]
  fn a_long_error_is_cut_to_its_first_300_code_points() {
    // 350 code points, the last 50 beyond the basic multilingual plane, so the cut counts characters, not bytes.
    let long = "é".to_string() + &"𐍈".repeat(174) + &"x".repeat(175);
    assert_eq!(long.chars().count(), 350);
    let event = SwitchEvent {
      phase: "failed".to_string(),
      from_session: None,
      to_session: "session-b".to_string(),
      generation: 1,
      winner_generation: None,
      elapsed_ms: None,
      error: Some(&long),
      client_ts: "2026-02-13T10:00:00Z".to_string(),
    };
    let cut = cut_error(Some(&long)).expect("the error is over the limit");
    let record = switch_record(&event, &cut);
    let (_, cut) = record.iter().find(|(name, _)| *name == "error").unwrap();
    let LogValue::Str(cut) = cut else { panic!("the error field is a string") };
    assert_eq!(cut.chars().count(), MAX_ERROR_CHARS);
    assert_eq!(*cut, long.chars().take(MAX_ERROR_CHARS).collect::<String>());
  }

  #[test]
  fn a_short_error_is_logged_whole_and_an_absent_one_logs_null() {
    let event = SwitchEvent {
      phase: "started".to_string(),
      from_session: None,
      to_session: "session-b".to_string(),
      generation: 1,
      winner_generation: None,
      elapsed_ms: None,
      error: None,
      client_ts: "2026-02-13T10:00:00Z".to_string(),
    };
    let record = switch_record(&event, "");
    let values: Vec<(&str, &LogValue)> = record.iter().map(|(name, value)| (*name, value)).collect();
    assert_eq!(values[5], ("elapsed_ms", &LogValue::Null));
    assert_eq!(values[6], ("error", &LogValue::Null));
    let named: Vec<&str> = record.iter().map(|(name, _)| *name).collect();
    assert_eq!(
      named,
      ["phase", "from_session", "to_session", "generation", "winner_generation", "elapsed_ms", "error", "client_ts"]
    );
  }
}
