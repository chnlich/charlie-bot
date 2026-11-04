//! The internal Feishu routes behind `charliebot feishu reply`, `read`, `download` and `check`.
//! Each body is validated before the handler runs, so a malformed request answers 422 whatever the session is. A
//! refusal of the handler answers its status with `{"detail": ...}`, and an unhandled failure answers 500.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::Request;
use axum::response::Response;
use charliebot_chat_threads::thread_entry::{ThreadDetail, ThreadReplyError};
use charliebot_compat::request_body::{self, BodyModel, Field, FieldType, RequestBody};
use charliebot_compat::responses;
use charliebot_compat::routing::post_only;
use charliebot_compat::validation::Bound;
use charliebot_registry::FeatureContext;
use charliebot_registry::caller;
use http::StatusCode;
use serde_json::{Map, Value, json};

use crate::adapter::{self, Bot};
use crate::client::BASE_URL;
use crate::commands;

/// `FeishuReadRequest.limit`'s default: 50, bounded to 1..=100.
const DEFAULT_READ_LIMIT: u64 = 50;

/// The download kinds the Open API resource endpoint takes.
const DOWNLOAD_KINDS: &[&str] = &["image", "file"];

/// `FeishuReplyRequest`: the calling session's id and the reply text.
const REPLY_FIELDS: &[Field] =
  &[Field::required("session_id", FieldType::String), Field::required("text", FieldType::String)];
const REPLY_BODY: BodyModel = BodyModel::required(REPLY_FIELDS).forbid_extra();

/// `FeishuReadRequest`: the calling session's id and the window size.
const READ_FIELDS: &[Field] = &[
  Field::required("session_id", FieldType::String),
  Field::optional("limit", FieldType::Integer).ge(Bound::Int(1)).le(Bound::Int(100)),
];
const READ_BODY: BodyModel = BodyModel::required(READ_FIELDS).forbid_extra();

/// `FeishuDownloadRequest`: the calling session's id, the message and resource keys, the kind, and the output path.
const DOWNLOAD_FIELDS: &[Field] = &[
  Field::required("session_id", FieldType::String),
  Field::required("message_id", FieldType::String),
  Field::required("file_key", FieldType::String),
  Field::required("kind", FieldType::Literal(DOWNLOAD_KINDS)),
  Field::required("output", FieldType::String),
];
const DOWNLOAD_BODY: BodyModel = BodyModel::required(DOWNLOAD_FIELDS).forbid_extra();

/// `FeishuCheckRequest`: no fields.
const CHECK_BODY: BodyModel = BodyModel::required(&[]).forbid_extra();

const REPLY_PATH: &str = "/api/internal/feishu/reply";
const READ_PATH: &str = "/api/internal/feishu/read";
const DOWNLOAD_PATH: &str = "/api/internal/feishu/download";
const CHECK_PATH: &str = "/api/internal/feishu/check";

/// The route table on Feishu's Open API base.
pub fn routes(features: &Arc<FeatureContext>) -> Router {
  routes_at(features, BASE_URL)
}

/// The route table with the Open API base `base_url`; the tests point it at a loopback fake.
pub fn routes_at(features: &Arc<FeatureContext>, base_url: &str) -> Router {
  Router::new()
    .route(REPLY_PATH, post_only(post_reply))
    .route(READ_PATH, post_only(post_read))
    .route(DOWNLOAD_PATH, post_only(post_download))
    .route(CHECK_PATH, post_only(post_check))
    .with_state(Bot::new(Arc::clone(features), base_url))
}

/// Whether a path is one of the Feishu routes, whatever the method (the trailing-slash redirect asks this).
pub fn matches(path: &str) -> bool {
  matches!(path, REPLY_PATH | READ_PATH | DOWNLOAD_PATH | CHECK_PATH)
}

/// `POST /api/internal/feishu/reply`: checks the thread is fresh, then posts the text to it.
async fn post_reply(State(bot): State<Bot>, request: Request<Body>) -> Response {
  let (parts, stream) = request.into_parts();
  let body = match request_body::read(&parts.headers, stream).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  let fields = match declared_fields(&body, &REPLY_BODY) {
    Ok(fields) => fields,
    Err(response) => return *response,
  };
  let session_id = text_field(&fields, "session_id");
  let text = text_field(&fields, "text");
  let identity = match caller::resolve_caller(&parts.headers, &bot.features) {
    Ok(identity) => identity,
    Err(response) => return response,
  };
  let (request_id, run_started_at) = match identity.claims() {
    Some(claims) => {
      let run = match bot.features.store.read_run_sync(&claims.session_id, &claims.run_id) {
        Ok(Some(run)) => run,
        Ok(None) => return responses::internal_error("RuntimeError", "verified caller run disappeared".to_string()),
        Err(error) => return responses::internal_error("RuntimeError", error.to_string()),
      };
      let started_at = run.started_at.expect("a verified run has a start timestamp").to_isoformat_text();
      (
        Some(charliebot_store::control_events::derived_agent_send_request_id(
          &claims.run_id,
          "feishu reply",
          &session_id,
          &text,
        )),
        Some(started_at),
      )
    }
    None => (None, None),
  };
  let result =
    adapter::post_reply_with_caller_context(&bot, &session_id, &text, request_id.as_deref(), run_started_at.as_deref())
      .await;
  answer(result)
}

/// `POST /api/internal/feishu/read`: the session's own thread, with the unread messages marked read.
async fn post_read(State(bot): State<Bot>, request: Request<Body>) -> Response {
  let (parts, stream) = request.into_parts();
  let body = match request_body::read(&parts.headers, stream).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  let fields = match declared_fields(&body, &READ_BODY) {
    Ok(fields) => fields,
    Err(response) => return *response,
  };
  let session_id = text_field(&fields, "session_id");
  let limit = fields.get("limit").and_then(Value::as_u64).unwrap_or(DEFAULT_READ_LIMIT) as usize;
  answer(commands::read_thread(&bot, &session_id, limit).await)
}

/// `POST /api/internal/feishu/download`: writes one image or file of the session's thread into the session directory.
async fn post_download(State(bot): State<Bot>, request: Request<Body>) -> Response {
  let (parts, stream) = request.into_parts();
  let body = match request_body::read(&parts.headers, stream).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  let fields = match declared_fields(&body, &DOWNLOAD_BODY) {
    Ok(fields) => fields,
    Err(response) => return *response,
  };
  answer(
    commands::download(
      &bot,
      &text_field(&fields, "session_id"),
      &text_field(&fields, "message_id"),
      &text_field(&fields, "file_key"),
      &text_field(&fields, "kind"),
      &text_field(&fields, "output"),
    )
    .await,
  )
}

/// `POST /api/internal/feishu/check`: the app's setup.
async fn post_check(State(bot): State<Bot>, request: Request<Body>) -> Response {
  let (parts, stream) = request.into_parts();
  let body = match request_body::read(&parts.headers, stream).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  if let Err(response) = declared_fields(&body, &CHECK_BODY) {
    return *response;
  }
  answer(commands::check_setup(&bot).await)
}

/// The declared fields of a body that validates against a required model.
fn declared_fields(body: &RequestBody, model: &BodyModel) -> Result<Map<String, Value>, Box<Response>> {
  match body.validate(model)? {
    Some(Value::Object(fields)) => Ok(fields),
    _ => unreachable!("a required model validates a body to its declared fields"),
  }
}

/// A string field of a validated body.
fn text_field(fields: &Map<String, Value>, name: &str) -> String {
  match fields.get(name).and_then(Value::as_str) {
    Some(text) => text.to_string(),
    None => unreachable!("a validated {name} is a string"),
  }
}

/// A handler's answer: the readback with 200, a refusal with its status and detail, or the unhandled failure's 500.
fn answer(result: Result<Value, ThreadReplyError>) -> Response {
  match result {
    Ok(body) => responses::json(StatusCode::OK, &body),
    Err(error) if error.status == StatusCode::INTERNAL_SERVER_ERROR => {
      responses::internal_error("RuntimeError", error.to_string())
    }
    Err(ThreadReplyError { status, detail }) => {
      let detail = match detail {
        ThreadDetail::Text(text) => Value::String(text),
        ThreadDetail::Structured(value) => value,
      };
      responses::json(status, &json!({"detail": detail}))
    }
  }
}
