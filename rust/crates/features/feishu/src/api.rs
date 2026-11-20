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
use charliebot_cli_common::channel_verb::{READ_LIMIT_DEFAULT, READ_LIMIT_MAX, READ_LIMIT_MIN, REPLY_BODY};
use charliebot_cli_common::fields::{LIMIT, SESSION_ID, TEXT};
use charliebot_compat::request_body::{self, BodyModel, Field, FieldType, text_field};
use charliebot_compat::routing::post_only;
use charliebot_compat::validation::Bound;
use charliebot_registry::FeatureContext;
use charliebot_registry::caller;
use http::StatusCode;
use serde_json::Value;

use crate::adapter::Bot;
use crate::client::BASE_URL;
use crate::fields::{
  DOWNLOAD_KINDS, FILE_KEY, KIND, MESSAGE_ID, OUTPUT, PAGE, READ_MODE, READ_MODE_CHAT, READ_MODE_TOPIC, READ_MODES,
  TOPIC_ID,
};

/// The read request's fields: the calling session, window size, and optional read mode parameters.
const READ_FIELDS: &[Field] = &[
  Field::required(SESSION_ID, FieldType::String),
  Field::optional(LIMIT, FieldType::Integer).ge(Bound::Int(READ_LIMIT_MIN)).le(Bound::Int(READ_LIMIT_MAX)),
  Field::optional(READ_MODE, FieldType::Literal(READ_MODES)),
  Field::optional(TOPIC_ID, FieldType::String),
  Field::optional(PAGE, FieldType::String),
];
const READ_BODY: BodyModel = BodyModel::required(READ_FIELDS).forbid_extra();

/// The download request's fields: the calling session's id, the message and resource keys, the kind, and the output
/// path.
const DOWNLOAD_FIELDS: &[Field] = &[
  Field::required(SESSION_ID, FieldType::String),
  Field::required(MESSAGE_ID, FieldType::String),
  Field::required(FILE_KEY, FieldType::String),
  Field::required(KIND, FieldType::Literal(DOWNLOAD_KINDS)),
  Field::required(OUTPUT, FieldType::String),
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

/// The route table with the Open API base `base_url`. The tests point it at a loopback fake.
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
  let text = text_field(&fields, TEXT);
  let identity = match caller::resolve_caller(&parts.headers, &bot.features) {
    Ok(identity) => identity,
    Err(response) => return response,
  };
  let (request_id, run_started_at) =
    match identity.reply_context(&bot.features.store, "feishu reply", &session_id, &text) {
      Ok(context) => context,
      Err(response) => return response,
    };
  let result =
    bot.post_reply_with_caller_context(&session_id, &text, request_id.as_deref(), run_started_at.as_deref()).await;
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
  let limit = fields.get(LIMIT).and_then(Value::as_u64).unwrap_or(READ_LIMIT_DEFAULT as u64) as usize;
  let mode = fields.get(READ_MODE).and_then(Value::as_str);
  let topic_id = fields.get(TOPIC_ID).and_then(Value::as_str);
  let page = fields.get(PAGE).and_then(Value::as_str);
  let result = match (mode, topic_id, page) {
    (None, None, None) => bot.read_thread(&session_id, limit).await,
    (Some(READ_MODE_CHAT), None, page) => bot.read_chat(&session_id, limit, page).await,
    (Some(READ_MODE_TOPIC), Some(thread_id), page) => bot.read_topic(&session_id, thread_id, limit, page).await,
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
    bot
      .download(
        &text_field(&fields, SESSION_ID),
        &text_field(&fields, MESSAGE_ID),
        &text_field(&fields, FILE_KEY),
        &text_field(&fields, KIND),
        &text_field(&fields, OUTPUT),
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
  reply_response(bot.check_setup().await)
}
