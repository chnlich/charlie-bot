//! The internal Feishu routes behind `charliebot feishu reply`, `read`, `download` and `check`.
//! Each body is validated before the handler runs, so a malformed request answers 422 whatever the session is. A
//! refusal of the handler answers its status with `{"detail": ...}`, and an unhandled failure answers 500.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::Request;
use axum::response::Response;
use charliebot_chat_threads::channel_service::reply_response;
use charliebot_chat_threads::thread_entry::ThreadReplyError;
use charliebot_cli_common::fields::SESSION_ID;
use charliebot_compat::request_body::{self, BodyModel, Field, FieldType, text_field};
use charliebot_compat::responses;
use charliebot_compat::routing::post_only;
use charliebot_compat::validation::Bound;
use charliebot_registry::FeatureContext;
use charliebot_registry::caller;
use http::StatusCode;
use serde_json::Value;

use crate::adapter::{self, Bot};
use crate::client::BASE_URL;
use crate::commands;
use crate::fields::MESSAGE_ID;

/// The read request's default window size: 50, bounded to 1..=100.
const DEFAULT_READ_LIMIT: u64 = 50;

/// The download kinds the Open API resource endpoint takes.
const DOWNLOAD_KINDS: &[&str] = &["image", "file"];

/// The reply request's fields: the calling session's id and the reply text.
const REPLY_FIELDS: &[Field] =
  &[Field::required(SESSION_ID, FieldType::String), Field::required("text", FieldType::String)];
const REPLY_BODY: BodyModel = BodyModel::required(REPLY_FIELDS).forbid_extra();

/// The read request's fields: the calling session, window size, and optional read mode parameters.
const READ_FIELDS: &[Field] = &[
  Field::required(SESSION_ID, FieldType::String),
  Field::optional("limit", FieldType::Integer).ge(Bound::Int(1)).le(Bound::Int(100)),
  Field::optional("mode", FieldType::Literal(&["chat", "topic"])),
  Field::optional("topic_id", FieldType::String),
  Field::optional("page", FieldType::String),
];
const READ_BODY: BodyModel = BodyModel::required(READ_FIELDS).forbid_extra();

/// The download request's fields: the calling session's id, the message and resource keys, the kind, and the output
/// path.
const DOWNLOAD_FIELDS: &[Field] = &[
  Field::required(SESSION_ID, FieldType::String),
  Field::required(MESSAGE_ID, FieldType::String),
  Field::required("file_key", FieldType::String),
  Field::required("kind", FieldType::Literal(DOWNLOAD_KINDS)),
  Field::required("output", FieldType::String),
];
const DOWNLOAD_BODY: BodyModel = BodyModel::required(DOWNLOAD_FIELDS).forbid_extra();

/// The check request's body: no fields.
const CHECK_BODY: BodyModel = BodyModel::required(&[]).forbid_extra();

/// The four internal route paths. The `cli` verbs build their requests from these names, so a rename here cannot
/// leave a verb posting to a path the router no longer serves.
pub(crate) const REPLY_PATH: &str = "/api/internal/feishu/reply";
pub(crate) const READ_PATH: &str = "/api/internal/feishu/read";
pub(crate) const DOWNLOAD_PATH: &str = "/api/internal/feishu/download";
pub(crate) const CHECK_PATH: &str = "/api/internal/feishu/check";

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

/// The reply route (`POST /api/internal/feishu/reply`): checks the thread is fresh, then posts the text to it.
async fn post_reply(State(bot): State<Bot>, request: Request<Body>) -> Response {
  let (parts, stream) = request.into_parts();
  let body = match request_body::read(&parts.headers, stream).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  let fields = match body.declared_fields(&REPLY_BODY) {
    Ok(fields) => fields,
    Err(response) => return *response,
  };
  let session_id = text_field(&fields, SESSION_ID);
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
  reply_response(result)
}

/// The read route (`POST /api/internal/feishu/read`): the session's topic or one of its read-only group views.
async fn post_read(State(bot): State<Bot>, request: Request<Body>) -> Response {
  let (parts, stream) = request.into_parts();
  let body = match request_body::read(&parts.headers, stream).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  let fields = match body.declared_fields(&READ_BODY) {
    Ok(fields) => fields,
    Err(response) => return *response,
  };
  let session_id = text_field(&fields, SESSION_ID);
  let limit = fields.get("limit").and_then(Value::as_u64).unwrap_or(DEFAULT_READ_LIMIT) as usize;
  let mode = fields.get("mode").and_then(Value::as_str);
  let topic_id = fields.get("topic_id").and_then(Value::as_str);
  let page = fields.get("page").and_then(Value::as_str);
  let result = match (mode, topic_id, page) {
    (None, None, None) => commands::read_thread(&bot, &session_id, limit).await,
    (Some("chat"), None, page) => commands::read_chat(&bot, &session_id, limit, page).await,
    (Some("topic"), Some(thread_id), page) => commands::read_topic(&bot, &session_id, thread_id, limit, page).await,
    _ => Err(ThreadReplyError::refused(
      StatusCode::UNPROCESSABLE_ENTITY,
      "read mode parameters do not match the selected mode",
    )),
  };
  reply_response(result)
}

/// The download route (`POST /api/internal/feishu/download`): writes one image or file from the session's group into
/// the session directory.
async fn post_download(State(bot): State<Bot>, request: Request<Body>) -> Response {
  let (parts, stream) = request.into_parts();
  let body = match request_body::read(&parts.headers, stream).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  let fields = match body.declared_fields(&DOWNLOAD_BODY) {
    Ok(fields) => fields,
    Err(response) => return *response,
  };
  reply_response(
    commands::download(
      &bot,
      &text_field(&fields, SESSION_ID),
      &text_field(&fields, MESSAGE_ID),
      &text_field(&fields, "file_key"),
      &text_field(&fields, "kind"),
      &text_field(&fields, "output"),
    )
    .await,
  )
}

/// The check route (`POST /api/internal/feishu/check`): the app's setup.
async fn post_check(State(bot): State<Bot>, request: Request<Body>) -> Response {
  let (parts, stream) = request.into_parts();
  let body = match request_body::read(&parts.headers, stream).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  if let Err(response) = body.declared_fields(&CHECK_BODY) {
    return *response;
  }
  reply_response(commands::check_setup(&bot).await)
}
