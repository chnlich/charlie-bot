//! The diag route: `POST /api/diag/client-events`, one validated client timing event per call,
//! logged as a single `diag_client` line carrying the event's `kind`, its fields and the browser's
//! `forwarded_for`. No session or state is consulted; the route answers `{"ok": true}`.
//!
//! The five kinds and their fields follow plan section 4.1 of the poor-network line: `switch`
//! carries the switch phases plus the bootstrap's Resource Timing, `send` one message send's
//! click-to-response and click-to-echo waits, `socket` a disconnect episode and one record per
//! subscription's catch-up, `page_load` the document's Navigation Timing with the open case, the
//! first chat render and catch-up, and the page's render-blocking and same-origin resource timings,
//! and `traffic` one tab's per-minute received bytes by endpoint.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::Request;
use axum::response::Response;
use charliebot_compat::forwarded::first_address;
use charliebot_compat::request_body::{
  self, BodyModel, Field, FieldType, integer_field, optional_text_field_ref, text_field, text_field_ref,
};
use charliebot_compat::responses;
use charliebot_compat::routing::post_only;
use charliebot_compat::validation::Bound;
use charliebot_core::event_types;
use charliebot_registry::FeatureContext;
use http::StatusCode;
use serde_json::{Map, Value, json};

/// The point logs at most this many code points of a switch's `error`, truncated to the first
/// `MAX_ERROR_CHARS`.
pub const MAX_ERROR_CHARS: usize = 300;

/// The largest endpoint path one traffic row carries; the client folds the ids out of the path.
const MAX_ENDPOINT_CHARS: usize = 200;

/// The largest number of endpoint rows one traffic event carries; the client sends at most this
/// many, the heaviest first.
const MAX_TRAFFIC_ENDPOINTS: usize = 40;

/// The largest length of `client_ts` and of the other timestamp strings the client sends.
const MAX_TIMESTAMP_CHARS: usize = 64;

/// The largest URL one page load's resource timing row carries.
const MAX_RESOURCE_URL_CHARS: usize = 2048;

/// The largest number of resource rows one page load carries, of each of its two lists.
const MAX_PAGE_LOAD_RESOURCES: usize = 16;

/// One client event kind: the five kinds the route accepts, with their wire names and body models.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EventKind {
  Switch,
  Send,
  Socket,
  PageLoad,
  Traffic,
}

impl EventKind {
  /// Every kind, in the envelope's declaration and log-line order.
  const ALL: [Self; 5] = [Self::Switch, Self::Send, Self::Socket, Self::PageLoad, Self::Traffic];

  /// The wire name the client sends and the log line carries.
  const fn as_str(self) -> &'static str {
    match self {
      Self::Switch => "switch",
      Self::Send => "send",
      Self::Socket => "socket",
      Self::PageLoad => "page_load",
      Self::Traffic => "traffic",
    }
  }

  /// The kind a wire name names, or None for a name outside the five.
  fn parse(name: &str) -> Option<Self> {
    Self::ALL.into_iter().find(|kind| kind.as_str() == name)
  }

  /// The body model that validates this kind's fields.
  fn body(self) -> &'static BodyModel {
    match self {
      Self::Switch => &SWITCH_BODY,
      Self::Send => &SEND_BODY,
      Self::Socket => &SOCKET_BODY,
      Self::PageLoad => &PAGE_LOAD_BODY,
      Self::Traffic => &TRAFFIC_BODY,
    }
  }
}

const KINDS: &[&str] = &[
  EventKind::Switch.as_str(),
  EventKind::Send.as_str(),
  EventKind::Socket.as_str(),
  EventKind::PageLoad.as_str(),
  EventKind::Traffic.as_str(),
];
const SOCKET_CAUSES: &[&str] = &["network"];
const PAGE_LOAD_CAUSES: &[&str] = &["open", "bootstrap_failure", "stale_asset"];
const OPEN_CASES: &[&str] = &["A", "B", "C"];
const CONTENT_SOURCES: &[&str] = &["network", "stored"];
const VISIBILITY_STATES: &[&str] = &["visible", "hidden", "prerender"];
const PHASES: &[&str] = &["started", "completed", "superseded", "failed", "render_error"];

/// The envelope every kind's body validates first: the `kind` selects the per-kind model, and
/// `client_ts` is the one field the log line carries whatever the kind. Extra keys stay, because
/// the per-kind model revalidates the whole body with `forbid_extra`.
const ENVELOPE_FIELDS: &[Field] = &[
  Field::required("kind", FieldType::Literal(KINDS)),
  Field::required(request_body::CLIENT_TS, FieldType::String).max_length(MAX_TIMESTAMP_CHARS),
];
const ENVELOPE_BODY: BodyModel = BodyModel::required(ENVELOPE_FIELDS);

/// The switch body: the fields the switch telemetry reports today, plus the bootstrap's Resource
/// Timing. A cache-hit switch or the `started` phase has no Resource Timing entry and sends nulls.
const SWITCH_FIELDS: &[Field] = &[
  Field::required("kind", FieldType::Literal(&[EventKind::Switch.as_str()])),
  Field::required("phase", FieldType::Literal(PHASES)),
  Field::required(event_types::FROM_SESSION, FieldType::NullableString),
  Field::required("to_session", FieldType::String),
  Field::required("generation", FieldType::Integer),
  Field::required("winner_generation", FieldType::NullableInteger),
  Field::optional("elapsed_ms", FieldType::NullableInteger).ge(Bound::Int(0)),
  Field::required("error", FieldType::NullableString),
  Field::required(request_body::CLIENT_TS, FieldType::String).max_length(MAX_TIMESTAMP_CHARS),
  Field::optional("request_start", FieldType::NullableInteger).ge(Bound::Int(0)),
  Field::optional("response_start", FieldType::NullableInteger).ge(Bound::Int(0)),
  Field::optional("response_end", FieldType::NullableInteger).ge(Bound::Int(0)),
  Field::optional("transfer_size", FieldType::NullableInteger).ge(Bound::Int(0)),
];
const SWITCH_BODY: BodyModel = BodyModel::required(SWITCH_FIELDS).forbid_extra();

/// The send body: one message send, from the click through the POST's response to the socket echo.
/// A refused POST gets no echo and sends a null `click_to_echo_ms`.
const SEND_FIELDS: &[Field] = &[
  Field::required("kind", FieldType::Literal(&[EventKind::Send.as_str()])),
  Field::required("request_id", FieldType::String).max_length(MAX_TIMESTAMP_CHARS),
  Field::optional("click_to_response_ms", FieldType::NullableInteger).ge(Bound::Int(0)),
  Field::optional("click_to_echo_ms", FieldType::NullableInteger).ge(Bound::Int(0)),
  Field::required("retries", FieldType::Integer).ge(Bound::Int(0)),
  Field::required(request_body::CLIENT_TS, FieldType::String).max_length(MAX_TIMESTAMP_CHARS),
];
const SEND_BODY: BodyModel = BodyModel::required(SEND_FIELDS).forbid_extra();

/// The socket body, in two flavors. A disconnect episode runs from the disconnect through the
/// reconnect to the replay's `catchup_complete`; an episode the page leaves before its replay
/// finished sends nulls for the reconnect and catch-up fields. A subscription record reports one
/// replay's completion — the subscription number, the session it served and its bytes — and sends
/// nulls for the disconnect fields. The only close cause left is `network`: a switch keeps the
/// socket and rides a subscribe frame instead of closing it.
const SOCKET_FIELDS: &[Field] = &[
  Field::required("kind", FieldType::Literal(&[EventKind::Socket.as_str()])),
  Field::optional("disconnect_at", FieldType::NullableString).max_length(MAX_TIMESTAMP_CHARS),
  Field::optional("cause", FieldType::NullableLiteral(SOCKET_CAUSES)),
  Field::optional("close_code", FieldType::NullableInteger).ge(Bound::Int(0)),
  Field::optional("reconnect_at", FieldType::NullableString).max_length(MAX_TIMESTAMP_CHARS),
  Field::optional("reconnect_count", FieldType::NullableInteger).ge(Bound::Int(0)),
  Field::optional("subscription", FieldType::NullableInteger).ge(Bound::Int(0)),
  Field::optional("session_id", FieldType::NullableString).max_length(MAX_TIMESTAMP_CHARS),
  Field::optional("catchup_at", FieldType::NullableString).max_length(MAX_TIMESTAMP_CHARS),
  Field::optional("catchup_bytes", FieldType::NullableInteger).ge(Bound::Int(0)),
  Field::required(request_body::CLIENT_TS, FieldType::String).max_length(MAX_TIMESTAMP_CHARS),
];
const SOCKET_BODY: BodyModel = BodyModel::required(SOCKET_FIELDS).forbid_extra();

/// One render-blocking file the page loaded from another origin: its Resource Timing in ms from
/// navigation start, and its body bytes. A null field is an entry the browser did not report.
const RENDER_BLOCKING_FIELDS: &[Field] = &[
  Field::required("url", FieldType::String).max_length(MAX_RESOURCE_URL_CHARS),
  Field::required("start_ms", FieldType::NullableInteger).ge(Bound::Int(0)),
  Field::required("response_end_ms", FieldType::NullableInteger).ge(Bound::Int(0)),
  Field::required("transfer_size", FieldType::NullableInteger).ge(Bound::Int(0)),
  Field::required("encoded_body_size", FieldType::NullableInteger).ge(Bound::Int(0)),
];
const RENDER_BLOCKING_BODY: BodyModel = BodyModel::required(RENDER_BLOCKING_FIELDS).forbid_extra();

/// One same-origin resource timing of the page load: the document or a same-origin render-blocking
/// file. The link estimate reads its round trip and throughput.
const SAME_ORIGIN_FIELDS: &[Field] = &[
  Field::required("url", FieldType::String).max_length(MAX_RESOURCE_URL_CHARS),
  Field::required("request_start_ms", FieldType::NullableInteger).ge(Bound::Int(0)),
  Field::required("response_start_ms", FieldType::NullableInteger).ge(Bound::Int(0)),
  Field::required("response_end_ms", FieldType::NullableInteger).ge(Bound::Int(0)),
  Field::required("encoded_body_size", FieldType::NullableInteger).ge(Bound::Int(0)),
];
const SAME_ORIGIN_BODY: BodyModel = BodyModel::required(SAME_ORIGIN_FIELDS).forbid_extra();

/// The page-load body: the document's Navigation Timing, once per page load, with the cause the
/// navigating path announced, the open case and the timings of the first chat render and of the
/// first catch-up, and the page's render-blocking and same-origin resource timings. The open case
/// and the content source are null on a page without a session.
const PAGE_LOAD_FIELDS: &[Field] = &[
  Field::required("kind", FieldType::Literal(&[EventKind::PageLoad.as_str()])),
  Field::required("response_end_ms", FieldType::Integer).ge(Bound::Int(0)),
  Field::required("dom_content_loaded_ms", FieldType::Integer).ge(Bound::Int(0)),
  Field::optional("transfer_size", FieldType::NullableInteger).ge(Bound::Int(0)),
  Field::required("cause", FieldType::Literal(PAGE_LOAD_CAUSES)),
  Field::required("newest_message_ms", FieldType::NullableInteger).ge(Bound::Int(0)),
  Field::required("current_ms", FieldType::NullableInteger).ge(Bound::Int(0)),
  Field::required("open_case", FieldType::NullableLiteral(OPEN_CASES)),
  Field::required("content_source", FieldType::NullableLiteral(CONTENT_SOURCES)),
  Field::required("offline_shell", FieldType::Boolean),
  Field::required("render_blocking", FieldType::ModelList { model: &RENDER_BLOCKING_BODY })
    .max_length(MAX_PAGE_LOAD_RESOURCES),
  Field::required("same_origin", FieldType::ModelList { model: &SAME_ORIGIN_BODY }).max_length(MAX_PAGE_LOAD_RESOURCES),
  Field::required(request_body::CLIENT_TS, FieldType::String).max_length(MAX_TIMESTAMP_CHARS),
];
const PAGE_LOAD_BODY: BodyModel = BodyModel::required(PAGE_LOAD_FIELDS).forbid_extra();

/// One traffic row: one endpoint's received bytes and request count over the event's window.
const TRAFFIC_ENDPOINT_FIELDS: &[Field] = &[
  Field::required("endpoint", FieldType::String).max_length(MAX_ENDPOINT_CHARS),
  Field::required("transfer_size", FieldType::Integer).ge(Bound::Int(0)),
  Field::required("count", FieldType::Integer).ge(Bound::Int(1)),
];
const TRAFFIC_ENDPOINT_BODY: BodyModel = BodyModel::required(TRAFFIC_ENDPOINT_FIELDS).forbid_extra();

/// The traffic body: one tab's received bytes over one window, grouped by endpoint. The client
/// flushes a window every minute through the page timer registry, so a hidden tab's window covers
/// the minutes it stayed hidden and `window_s` carries the seconds it observed.
const TRAFFIC_FIELDS: &[Field] = &[
  Field::required("kind", FieldType::Literal(&[EventKind::Traffic.as_str()])),
  Field::required("window_s", FieldType::Integer).ge(Bound::Int(1)),
  Field::required("endpoints", FieldType::ModelList { model: &TRAFFIC_ENDPOINT_BODY })
    .max_length(MAX_TRAFFIC_ENDPOINTS),
  Field::required("visibility_state", FieldType::Literal(VISIBILITY_STATES)),
  Field::required(request_body::CLIENT_TS, FieldType::String).max_length(MAX_TIMESTAMP_CHARS),
];
const TRAFFIC_BODY: BodyModel = BodyModel::required(TRAFFIC_FIELDS).forbid_extra();

/// The routes, with the feature context as their state. The path has one method.
pub fn routes(features: &Arc<FeatureContext>) -> Router {
  Router::new().route("/api/diag/client-events", post_only(post_client_event)).with_state(Arc::clone(features))
}

/// Whether a path is one of the diag routes, whatever the method. The server's trailing-slash redirect asks this of
/// the slash-toggled path.
pub fn matches(path: &str) -> bool {
  matches!(path, "/api/diag/client-events")
}

/// `POST /api/diag/client-events`: validate the event, log one `diag_client` line, answer `{"ok": true}`.
async fn post_client_event(State(_features): State<Arc<FeatureContext>>, request: Request<Body>) -> Response {
  let (parts, stream) = request.into_parts();
  let body = match request_body::read(&parts.headers, stream).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  let envelope = match body.validate(&ENVELOPE_BODY) {
    Ok(Some(Value::Object(values))) => values,
    Ok(_) => unreachable!("a required model validates a body to its fields"),
    Err(response) => return *response,
  };
  let kind = EventKind::parse(text_field_ref(&envelope, "kind")).expect("the envelope validated the kind");
  // The envelope drops the keys it does not declare, so the kind model revalidates the body
  // itself: only this pass's validated values reach the log line.
  let values = match body.validate(kind.body()) {
    Ok(Some(Value::Object(values))) => values,
    Ok(_) => unreachable!("a required model validates a body to its fields"),
    Err(response) => return *response,
  };
  log_client_event(kind, &values, &first_address(&parts.headers));
  responses::json(StatusCode::OK, &json!({"ok": true}))
}

/// The one `diag_client` line of the event's kind, its fields in declaration order and the
/// browser's address last.
fn log_client_event(kind: EventKind, values: &Map<String, Value>, forwarded_for: &str) {
  match kind {
    EventKind::Switch => log_switch(values, forwarded_for),
    EventKind::Send => log_send(values, forwarded_for),
    EventKind::Socket => log_socket(values, forwarded_for),
    EventKind::PageLoad => log_page_load(values, forwarded_for),
    EventKind::Traffic => log_traffic(values, forwarded_for),
  }
}

/// `error` cut to its first `MAX_ERROR_CHARS` code points. The check borrows the validated string and inspects
/// only the first character past the limit before allocating the retained prefix.
pub fn cut_error<'a>(error: Option<&'a str>) -> Option<std::borrow::Cow<'a, str>> {
  use std::borrow::Cow;
  error.map(|error| match error.char_indices().nth(MAX_ERROR_CHARS) {
    Some((end, _)) => Cow::Owned(error[..end].to_owned()),
    None => Cow::Borrowed(error),
  })
}

/// One loggable value: a string or integer as itself, and a missing optional as `null`, which the
/// console renderer spells `None` for and the JSON line spells `null`.
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

fn log_switch(values: &Map<String, Value>, forwarded_for: &str) {
  let error = cut_error(optional_text_field_ref(values, "error"));
  tracing::info!(
    kind = EventKind::Switch.as_str(),
    phase = %text_field(values, "phase"),
    from_session = %LogValue::of(optional_text_field_ref(values, event_types::FROM_SESSION)),
    to_session = %text_field(values, "to_session"),
    generation = %integer_field(values, "generation").expect("a required integer validates to a value"),
    winner_generation = %LogValue::of_int(integer_field(values, "winner_generation")),
    elapsed_ms = %LogValue::of_int(integer_field(values, "elapsed_ms")),
    error = %LogValue::of(error.as_deref()),
    client_ts = %text_field(values, request_body::CLIENT_TS),
    request_start = %LogValue::of_int(integer_field(values, "request_start")),
    response_start = %LogValue::of_int(integer_field(values, "response_start")),
    response_end = %LogValue::of_int(integer_field(values, "response_end")),
    transfer_size = %LogValue::of_int(integer_field(values, "transfer_size")),
    forwarded_for = %forwarded_for,
    "diag_client"
  );
}

fn log_send(values: &Map<String, Value>, forwarded_for: &str) {
  tracing::info!(
    kind = EventKind::Send.as_str(),
    request_id = %text_field(values, "request_id"),
    click_to_response_ms = %LogValue::of_int(integer_field(values, "click_to_response_ms")),
    click_to_echo_ms = %LogValue::of_int(integer_field(values, "click_to_echo_ms")),
    retries = %integer_field(values, "retries").expect("a required integer validates to a value"),
    client_ts = %text_field(values, request_body::CLIENT_TS),
    forwarded_for = %forwarded_for,
    "diag_client"
  );
}

fn log_socket(values: &Map<String, Value>, forwarded_for: &str) {
  tracing::info!(
    kind = EventKind::Socket.as_str(),
    disconnect_at = %LogValue::of(optional_text_field_ref(values, "disconnect_at")),
    cause = %LogValue::of(optional_text_field_ref(values, "cause")),
    close_code = %LogValue::of_int(integer_field(values, "close_code")),
    reconnect_at = %LogValue::of(optional_text_field_ref(values, "reconnect_at")),
    reconnect_count = %LogValue::of_int(integer_field(values, "reconnect_count")),
    subscription = %LogValue::of_int(integer_field(values, "subscription")),
    session_id = %LogValue::of(optional_text_field_ref(values, "session_id")),
    catchup_at = %LogValue::of(optional_text_field_ref(values, "catchup_at")),
    catchup_bytes = %LogValue::of_int(integer_field(values, "catchup_bytes")),
    client_ts = %text_field(values, request_body::CLIENT_TS),
    forwarded_for = %forwarded_for,
    "diag_client"
  );
}

fn log_page_load(values: &Map<String, Value>, forwarded_for: &str) {
  tracing::info!(
    kind = EventKind::PageLoad.as_str(),
    response_end_ms = %integer_field(values, "response_end_ms")
      .expect("a required integer validates to a value"),
    dom_content_loaded_ms = %integer_field(values, "dom_content_loaded_ms")
      .expect("a required integer validates to a value"),
    transfer_size = %LogValue::of_int(integer_field(values, "transfer_size")),
    cause = %text_field(values, "cause"),
    newest_message_ms = %LogValue::of_int(integer_field(values, "newest_message_ms")),
    current_ms = %LogValue::of_int(integer_field(values, "current_ms")),
    open_case = %LogValue::of(optional_text_field_ref(values, "open_case")),
    content_source = %LogValue::of(optional_text_field_ref(values, "content_source")),
    offline_shell = %values.get("offline_shell").expect("a required boolean validates to a value"),
    render_blocking = %values.get("render_blocking").expect("a required list validates to a value"),
    same_origin = %values.get("same_origin").expect("a required list validates to a value"),
    client_ts = %text_field(values, request_body::CLIENT_TS),
    forwarded_for = %forwarded_for,
    "diag_client"
  );
}

fn log_traffic(values: &Map<String, Value>, forwarded_for: &str) {
  tracing::info!(
    kind = EventKind::Traffic.as_str(),
    window_s = %integer_field(values, "window_s").expect("a required integer validates to a value"),
    endpoints = %values.get("endpoints").expect("a required field validates to a value"),
    visibility_state = %text_field(values, "visibility_state"),
    client_ts = %text_field(values, request_body::CLIENT_TS),
    forwarded_for = %forwarded_for,
    "diag_client"
  );
}

#[cfg(test)]
mod tests {
  use std::fmt;
  use std::sync::{Arc, Mutex};

  use charliebot_compat::request_body::RequestBody;
  use charliebot_compat::validation::{FieldError, Kind, Loc};
  use charliebot_core::test_support::{EventSink, install_events};
  use serde_json::json;
  use tracing::Event;
  use tracing::field::{Field as TracingField, Visit};

  use super::*;

  fn validate(body: Value, model: &BodyModel) -> Result<Value, Vec<FieldError>> {
    RequestBody::Json(body).check(model).map(|value| value.expect("a required model validates an object"))
  }

  /// The two passes the route runs: the envelope selects the kind, and the kind model validates
  /// the body itself — the envelope's validated values drop the keys it does not declare, so the
  /// kind model's errors and values come from the body, not the envelope's stripped object.
  fn validate_event(body: Value) -> Result<Value, Vec<FieldError>> {
    let envelope = validate(body.clone(), &ENVELOPE_BODY)?;
    let kind = EventKind::parse(envelope["kind"].as_str().expect("the envelope validated the kind"))
      .expect("the envelope validated the kind");
    validate(body, kind.body())
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

  /// The base body of each kind, every field set.
  fn base_body(kind: EventKind) -> Value {
    match kind {
      EventKind::Switch => json!({
        "kind": kind.as_str(),
        "phase": "completed",
        "from_session": "session-a",
        "to_session": "session-b",
        "generation": 3,
        "winner_generation": 2,
        "elapsed_ms": 1500,
        "error": null,
        "client_ts": "2026-02-13T10:00:00Z",
        "request_start": 100,
        "response_start": 400,
        "response_end": 1500,
        "transfer_size": 26592,
      }),
      EventKind::Send => json!({
        "kind": kind.as_str(),
        "request_id": "0f0e1d2c-3b4a-5968-7789-aabbccddeeff",
        "click_to_response_ms": 300,
        "click_to_echo_ms": 620,
        "retries": 1,
        "client_ts": "2026-02-13T10:00:00Z",
      }),
      EventKind::Socket => json!({
        "kind": kind.as_str(),
        "disconnect_at": "2026-02-13T09:59:00Z",
        "cause": "network",
        "close_code": 1006,
        "reconnect_at": "2026-02-13T09:59:02Z",
        "reconnect_count": 1,
        "catchup_at": "2026-02-13T09:59:03Z",
        "catchup_bytes": 80860,
        "client_ts": "2026-02-13T09:59:03Z",
      }),
      EventKind::PageLoad => json!({
        "kind": kind.as_str(),
        "response_end_ms": 33000,
        "dom_content_loaded_ms": 34500,
        "transfer_size": 415115,
        "cause": "bootstrap_failure",
        "newest_message_ms": 1200,
        "current_ms": 1900,
        "open_case": "A",
        "content_source": "network",
        "offline_shell": false,
        "render_blocking": [{
          "url": "https://cdn.jsdelivr.net/npm/marked/marked.min.js",
          "start_ms": 400,
          "response_end_ms": 900,
          "transfer_size": 2500,
          "encoded_body_size": 2500,
        }],
        "same_origin": [{
          "url": "https://page.example/",
          "request_start_ms": 0,
          "response_start_ms": 300,
          "response_end_ms": 1300,
          "encoded_body_size": 12500,
        }],
        "client_ts": "2026-02-13T10:00:00Z",
      }),
      EventKind::Traffic => json!({
        "kind": kind.as_str(),
        "window_s": 60,
        "endpoints": [
          {"endpoint": "/api/sessions/{id}/", "transfer_size": 113004, "count": 1},
          {"endpoint": "/api/concurrency/now", "transfer_size": 365, "count": 12},
        ],
        "visibility_state": "visible",
        "client_ts": "2026-02-13T10:00:00Z",
      }),
    }
  }

  // ---------------------------------------------------------------------------
  // Validation: the envelope selects the kind, and each kind validates to its fields
  // ---------------------------------------------------------------------------

  #[test]
  fn the_envelope_selects_each_kind_and_the_kind_model_validates() {
    for kind in EventKind::ALL {
      validate_event(base_body(kind)).unwrap_or_else(|errors| panic!("{} is valid: {errors:?}", kind.as_str()));
    }
  }

  #[test]
  fn an_unknown_kind_is_a_literal_error_at_the_kind() {
    let mut body = base_body(EventKind::Switch);
    body["kind"] = json!("cancelled");
    let errors = validate(body, &ENVELOPE_BODY).expect_err("the kind is unknown");
    assert_eq!(loc(&errors[0]), ["body", "kind"]);
    assert!(matches!(errors[0].kind, Kind::Literal(_)));
  }

  #[test]
  fn a_missing_client_ts_is_an_envelope_error_before_the_kind_model_runs() {
    let mut body = base_body(EventKind::Switch);
    body.as_object_mut().unwrap().remove("client_ts");
    let errors = validate(body, &ENVELOPE_BODY).expect_err("client_ts is missing");
    assert_eq!(loc(&errors[0]), ["body", "client_ts"]);
    assert_eq!(errors[0].kind, Kind::Missing);
  }

  #[test]
  fn an_extra_key_is_forbidden_by_the_kind_model() {
    let mut body = base_body(EventKind::Send);
    body["stale"] = json!(true);
    let errors = validate_event(body).expect_err("the key is undeclared");
    assert_eq!(loc(&errors[0]), ["body", "stale"]);
    assert_eq!(errors[0].kind, Kind::ExtraForbidden);
  }

  #[test]
  fn the_switch_fields_validate_in_declaration_order() {
    let values = validate_event(base_body(EventKind::Switch)).expect("the switch body is valid");
    let names: Vec<&str> = values.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
      names,
      [
        "kind",
        "phase",
        "from_session",
        "to_session",
        "generation",
        "winner_generation",
        "elapsed_ms",
        "error",
        "client_ts",
        "request_start",
        "response_start",
        "response_end",
        "transfer_size",
      ]
    );
    assert_eq!(values["transfer_size"], 26592);
  }

  #[test]
  fn a_started_switch_without_resource_timing_and_null_fields_validates() {
    let mut body = base_body(EventKind::Switch);
    body["phase"] = json!("started");
    body["winner_generation"] = json!(null);
    body["elapsed_ms"] = json!(0);
    for name in ["request_start", "response_start", "response_end", "transfer_size"] {
      body[name] = json!(null);
    }
    let values = validate_event(body).expect("the started body is valid");
    assert_eq!(values["request_start"], json!(null), "an explicit null optional stays null in the validated values");
    assert_eq!(values["elapsed_ms"], 0);
  }

  #[test]
  fn a_negative_elapsed_ms_violates_its_ge_bound() {
    let mut body = base_body(EventKind::Switch);
    body["elapsed_ms"] = json!(-1);
    let errors = validate_event(body).expect_err("the elapsed time is negative");
    assert_eq!(loc(&errors[0]), ["body", "elapsed_ms"]);
    assert_eq!(errors[0].kind, Kind::GreaterThanEqual(Bound::Int(0)));
  }

  #[test]
  fn an_unknown_socket_cause_and_an_unknown_page_load_cause_are_literal_errors() {
    let mut socket = base_body(EventKind::Socket);
    socket["cause"] = json!("deploy");
    let errors = validate_event(socket).expect_err("the socket cause is unknown");
    assert_eq!(loc(&errors[0]), ["body", "cause"]);
    assert!(matches!(errors[0].kind, Kind::Literal(_)));

    // A switch no longer closes the socket — it rides a subscribe frame — so the close cause is
    // refused with the rest.
    let mut socket = base_body(EventKind::Socket);
    socket["cause"] = json!("switch");
    let errors = validate_event(socket).expect_err("the switch close cause is retired");
    assert_eq!(loc(&errors[0]), ["body", "cause"]);

    let mut page_load = base_body(EventKind::PageLoad);
    page_load["cause"] = json!("crash");
    let errors = validate_event(page_load).expect_err("the page load cause is unknown");
    assert_eq!(loc(&errors[0]), ["body", "cause"]);
    assert!(matches!(errors[0].kind, Kind::Literal(_)));
  }

  #[test]
  fn a_traffic_endpoint_row_validates_as_the_nested_model() {
    let mut body = base_body(EventKind::Traffic);
    body["endpoints"] = json!([{"endpoint": "/api/sessions/{id}/", "transfer_size": 113004}]);
    let errors = validate_event(body).expect_err("the row misses count");
    assert_eq!(loc(&errors[0]), ["body", "endpoints", "0", "count"]);
    assert_eq!(errors[0].kind, Kind::Missing);
  }

  #[test]
  fn a_traffic_event_over_forty_endpoints_is_too_long() {
    let mut body = base_body(EventKind::Traffic);
    body["endpoints"] = json!(
      (0..41)
        .map(|index| json!({"endpoint": format!("/api/p{index}"), "transfer_size": 1, "count": 1}))
        .collect::<Vec<_>>()
    );
    let errors = validate_event(body).expect_err("the endpoint list is over the limit");
    assert_eq!(loc(&errors[0]), ["body", "endpoints"]);
    assert!(matches!(errors[0].kind, Kind::ListTooLong { max_length: MAX_TRAFFIC_ENDPOINTS, .. }));
  }

  #[test]
  fn unsigned_values_above_i64_max_are_preserved() {
    let mut body = base_body(EventKind::Switch);
    body["generation"] = json!(i64::MAX as u64 + 1);
    body["winner_generation"] = json!(u64::MAX);
    let values = validate_event(body).expect("the switch body is valid");
    assert_eq!(values["generation"], json!(i64::MAX as u64 + 1));
    assert_eq!(values["winner_generation"], json!(u64::MAX));
    let error = cut_error(optional_text_field_ref(values.as_object().unwrap(), "error"));
    assert!(error.is_none());
  }

  #[test]
  fn a_long_error_is_cut_to_its_first_300_code_points() {
    // 350 code points, the last 50 beyond the basic multilingual plane, so the cut counts characters, not bytes.
    let long = "é".to_string() + &"𐍈".repeat(174) + &"x".repeat(175);
    assert_eq!(long.chars().count(), 350);
    let cut = cut_error(Some(&long)).expect("the error is over the limit");
    assert_eq!(cut.chars().count(), MAX_ERROR_CHARS);
    assert_eq!(cut, long.chars().take(MAX_ERROR_CHARS).collect::<String>());
    let short = cut_error(Some("short diagnostic")).expect("the error is present");
    assert!(matches!(short, std::borrow::Cow::Borrowed(_)));
  }

  // ---------------------------------------------------------------------------
  // The diag_client log line: one line per kind, its fields and the forwarded address
  // ---------------------------------------------------------------------------

  /// The `(name, value)` pairs of one event, in the order the event records them.
  type Recorded = Vec<(String, String)>;

  #[derive(Default)]
  struct Visitor(Recorded);

  impl Visit for Visitor {
    fn record_str(&mut self, field: &TracingField, value: &str) {
      self.0.push((field.name().to_string(), value.to_string()));
    }

    fn record_i128(&mut self, field: &TracingField, value: i128) {
      self.0.push((field.name().to_string(), value.to_string()));
    }

    fn record_u64(&mut self, field: &TracingField, value: u64) {
      self.0.push((field.name().to_string(), value.to_string()));
    }

    fn record_debug(&mut self, field: &TracingField, value: &dyn fmt::Debug) {
      self.0.push((field.name().to_string(), format!("{value:?}")));
    }
  }

  struct Capture(Arc<Mutex<Vec<Recorded>>>);

  impl EventSink for Capture {
    fn event(&self, event: &Event<'_>) {
      let mut visitor = Visitor::default();
      event.record(&mut visitor);
      self.0.lock().unwrap().push(visitor.0);
    }
  }

  /// Logs `body` through the handler path and returns the recorded `diag_client` event.
  fn logged_event(body: Value) -> Recorded {
    let events = Arc::new(Mutex::new(Vec::new()));
    let _capture = install_events(Arc::new(Capture(Arc::clone(&events))));
    let values = validate_event(body).expect("the event body is valid");
    let kind = EventKind::parse(values["kind"].as_str().expect("the kind validates to a string"))
      .expect("the envelope validated the kind");
    log_client_event(kind, values.as_object().unwrap(), "100.64.0.8");
    let events = events.lock().unwrap();
    assert_eq!(events.len(), 1, "one diag_client event per call");
    events[0].clone()
  }

  fn field<'a>(event: &'a Recorded, name: &str) -> &'a str {
    event
      .iter()
      .find_map(|(field, value)| (field == name).then_some(value.as_str()))
      .unwrap_or_else(|| panic!("the diag_client event has no {name} field"))
  }

  #[test]
  fn a_switch_event_logs_one_diag_client_line_with_its_fields_and_the_forwarded_address() {
    let event = logged_event(base_body(EventKind::Switch));
    assert_eq!(field(&event, "kind"), EventKind::Switch.as_str());
    assert_eq!(field(&event, "phase"), "completed");
    assert_eq!(field(&event, "from_session"), "session-a");
    assert_eq!(field(&event, "to_session"), "session-b");
    assert_eq!(field(&event, "generation"), "3");
    assert_eq!(field(&event, "winner_generation"), "2");
    assert_eq!(field(&event, "elapsed_ms"), "1500");
    assert_eq!(field(&event, "error"), "null");
    assert_eq!(field(&event, "client_ts"), "2026-02-13T10:00:00Z");
    assert_eq!(field(&event, "request_start"), "100");
    assert_eq!(field(&event, "response_start"), "400");
    assert_eq!(field(&event, "response_end"), "1500");
    assert_eq!(field(&event, "transfer_size"), "26592");
    assert_eq!(field(&event, "forwarded_for"), "100.64.0.8");
  }

  #[test]
  fn a_send_event_logs_one_diag_client_line_with_its_fields_and_the_forwarded_address() {
    let event = logged_event(base_body(EventKind::Send));
    assert_eq!(field(&event, "kind"), EventKind::Send.as_str());
    assert_eq!(field(&event, "request_id"), "0f0e1d2c-3b4a-5968-7789-aabbccddeeff");
    assert_eq!(field(&event, "click_to_response_ms"), "300");
    assert_eq!(field(&event, "click_to_echo_ms"), "620");
    assert_eq!(field(&event, "retries"), "1");
    assert_eq!(field(&event, "client_ts"), "2026-02-13T10:00:00Z");
    assert_eq!(field(&event, "forwarded_for"), "100.64.0.8");
  }

  #[test]
  fn a_socket_event_logs_one_diag_client_line_with_its_fields_and_the_forwarded_address() {
    let event = logged_event(base_body(EventKind::Socket));
    assert_eq!(field(&event, "kind"), EventKind::Socket.as_str());
    assert_eq!(field(&event, "disconnect_at"), "2026-02-13T09:59:00Z");
    assert_eq!(field(&event, "cause"), "network");
    assert_eq!(field(&event, "close_code"), "1006");
    assert_eq!(field(&event, "reconnect_at"), "2026-02-13T09:59:02Z");
    assert_eq!(field(&event, "reconnect_count"), "1");
    assert_eq!(field(&event, "subscription"), "null", "the episode flavor names no subscription");
    assert_eq!(field(&event, "session_id"), "null");
    assert_eq!(field(&event, "catchup_at"), "2026-02-13T09:59:03Z");
    assert_eq!(field(&event, "catchup_bytes"), "80860");
    assert_eq!(field(&event, "client_ts"), "2026-02-13T09:59:03Z");
    assert_eq!(field(&event, "forwarded_for"), "100.64.0.8");
  }

  #[test]
  fn a_subscription_catchup_logs_one_socket_line_without_disconnect_fields() {
    let event = logged_event(json!({
      "kind": EventKind::Socket.as_str(),
      "subscription": 3,
      "session_id": "session-b",
      "catchup_at": "2026-02-13T10:00:01Z",
      "catchup_bytes": 2048,
      "client_ts": "2026-02-13T10:00:01Z",
    }));
    assert_eq!(field(&event, "kind"), EventKind::Socket.as_str());
    assert_eq!(field(&event, "disconnect_at"), "null", "a subscription record reports no disconnect");
    assert_eq!(field(&event, "cause"), "null");
    assert_eq!(field(&event, "reconnect_at"), "null");
    assert_eq!(field(&event, "subscription"), "3");
    assert_eq!(field(&event, "session_id"), "session-b");
    assert_eq!(field(&event, "catchup_bytes"), "2048");
  }

  #[test]
  fn a_socket_episode_missing_its_disconnect_fields_still_validates() {
    let event = logged_event(json!({
      "kind": EventKind::Socket.as_str(),
      "disconnect_at": "2026-02-13T09:59:00Z",
      "cause": "network",
      "close_code": 1006,
      "reconnect_at": null,
      "reconnect_count": null,
      "catchup_at": null,
      "catchup_bytes": null,
      "client_ts": "2026-02-13T09:59:00Z",
    }));
    assert_eq!(field(&event, "reconnect_at"), "null");
    assert_eq!(field(&event, "catchup_bytes"), "null");
  }

  #[test]
  fn a_page_load_event_logs_one_diag_client_line_with_its_fields_and_the_forwarded_address() {
    let event = logged_event(base_body(EventKind::PageLoad));
    assert_eq!(field(&event, "kind"), EventKind::PageLoad.as_str());
    assert_eq!(field(&event, "response_end_ms"), "33000");
    assert_eq!(field(&event, "dom_content_loaded_ms"), "34500");
    assert_eq!(field(&event, "transfer_size"), "415115");
    assert_eq!(field(&event, "cause"), "bootstrap_failure");
    assert_eq!(field(&event, "newest_message_ms"), "1200");
    assert_eq!(field(&event, "current_ms"), "1900");
    assert_eq!(field(&event, "open_case"), "A");
    assert_eq!(field(&event, "content_source"), "network");
    assert_eq!(field(&event, "offline_shell"), "false");
    assert!(field(&event, "render_blocking").contains("marked.min.js"));
    assert!(field(&event, "same_origin").contains("page.example"));
    assert_eq!(field(&event, "client_ts"), "2026-02-13T10:00:00Z");
    assert_eq!(field(&event, "forwarded_for"), "100.64.0.8");
  }

  #[test]
  fn a_page_load_without_a_session_validates_with_null_case_source_and_timings() {
    let mut page_load = base_body(EventKind::PageLoad);
    page_load["newest_message_ms"] = Value::Null;
    page_load["current_ms"] = Value::Null;
    page_load["open_case"] = Value::Null;
    page_load["content_source"] = Value::Null;
    validate_event(page_load).expect("a page with no session carries nulls");
  }

  #[test]
  fn an_unknown_open_case_is_a_literal_error_at_the_field() {
    let mut page_load = base_body(EventKind::PageLoad);
    page_load["open_case"] = json!("D");
    let errors = validate_event(page_load).expect_err("D is not an open case");
    assert_eq!(loc(&errors[0]), ["body", "open_case"]);
    assert!(matches!(errors[0].kind, Kind::Literal(_)));
  }

  #[test]
  fn a_render_blocking_row_without_a_url_is_a_missing_field() {
    let mut page_load = base_body(EventKind::PageLoad);
    page_load["render_blocking"][0].as_object_mut().expect("a row").remove("url");
    let errors = validate_event(page_load).expect_err("the row has no url");
    assert_eq!(errors[0].kind, Kind::Missing);
  }

  #[test]
  fn a_traffic_event_logs_one_diag_client_line_with_its_rows_and_the_forwarded_address() {
    let event = logged_event(base_body(EventKind::Traffic));
    assert_eq!(field(&event, "kind"), EventKind::Traffic.as_str());
    assert_eq!(field(&event, "window_s"), "60");
    assert_eq!(
      field(&event, "endpoints"),
      r#"[{"endpoint":"/api/sessions/{id}/","transfer_size":113004,"count":1},"#.to_string()
        + r#"{"endpoint":"/api/concurrency/now","transfer_size":365,"count":12}]"#
    );
    assert_eq!(field(&event, "visibility_state"), "visible");
    assert_eq!(field(&event, "client_ts"), "2026-02-13T10:00:00Z");
    assert_eq!(field(&event, "forwarded_for"), "100.64.0.8");
  }

  #[test]
  fn a_started_switch_logs_null_resource_timing_and_a_null_error() {
    let mut body = base_body(EventKind::Switch);
    body["phase"] = json!("started");
    body["winner_generation"] = json!(null);
    for name in ["request_start", "response_start", "response_end", "transfer_size"] {
      body[name] = json!(null);
    }
    let event = logged_event(body);
    assert_eq!(field(&event, "winner_generation"), "null");
    assert_eq!(field(&event, "request_start"), "null");
    assert_eq!(field(&event, "transfer_size"), "null");
    assert_eq!(field(&event, "error"), "null");
  }

  #[test]
  fn a_long_error_is_logged_cut_to_its_first_300_code_points() {
    let long = "é".to_string() + &"𐍈".repeat(174) + &"x".repeat(175);
    let mut body = base_body(EventKind::Switch);
    body["error"] = json!(long);
    let event = logged_event(body);
    assert_eq!(field(&event, "error").chars().count(), MAX_ERROR_CHARS);
  }

  // ---------------------------------------------------------------------------
  // The route: one path, one method, the old switch-events path gone
  // ---------------------------------------------------------------------------

  #[test]
  fn the_route_matches_only_client_events() {
    assert!(matches("/api/diag/client-events"));
    assert!(!matches("/api/diag/switch-events"));
    assert!(!matches("/api/diag/client-events/"));
  }
}
