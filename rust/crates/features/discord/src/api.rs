//! The internal Discord routes behind `charliebot discord reply`, `read` and `check`.
//! Each body is validated before the handler runs, so a malformed request answers 422 whatever the session is. A
//! refusal of the handler answers its status with `{"detail": ...}`, and an unhandled failure answers 500.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::Request;
use axum::response::Response;
use charliebot_chat_threads::channel_service::{self, reply_response};
use charliebot_cli_common::channel_verb::{READ_LIMIT_DEFAULT, READ_LIMIT_MAX, READ_LIMIT_MIN};
use charliebot_cli_common::fields::{LIMIT, SESSION_ID};
use charliebot_compat::request_body::{self, BodyModel, Field, FieldType, text_field};
use charliebot_compat::routing::post_only;
use charliebot_compat::validation::Bound;
use charliebot_registry::FeatureContext;
use serde_json::Value;

use crate::discord_client::BASE_URL;
use crate::discord_listener::Bot;

/// `DiscordReadRequest`: the calling session's id, an optional channel link, and the window size.
const READ_FIELDS: &[Field] = &[
  Field::required(SESSION_ID, FieldType::String),
  Field::optional("url", FieldType::NullableString),
  Field::optional(LIMIT, FieldType::Integer).ge(Bound::Int(READ_LIMIT_MIN)).le(Bound::Int(READ_LIMIT_MAX)),
];
const READ_BODY: BodyModel = BodyModel::required(READ_FIELDS).forbid_extra();

/// The three internal route paths. The `cli` verbs build their requests from these names, so a rename here cannot
/// leave a verb posting to a path the router no longer serves.
pub(crate) const REPLY_PATH: &str = "/api/internal/discord/reply";
pub(crate) const READ_PATH: &str = "/api/internal/discord/read";
pub(crate) const CHECK_PATH: &str = "/api/internal/discord/check";

/// The route table on Discord's REST base.
pub fn routes(features: &Arc<FeatureContext>) -> Router {
  routes_at(features, BASE_URL)
}

/// The route table with the REST base `base_url`. The tests point it at a loopback fake.
pub fn routes_at(features: &Arc<FeatureContext>, base_url: &str) -> Router {
  Router::new()
    .route(REPLY_PATH, post_only(post_reply))
    .route(READ_PATH, post_only(post_read))
    .route(CHECK_PATH, post_only(post_check))
    .with_state(Bot::new(Arc::clone(features), base_url))
}

/// Whether a path is one of the Discord routes, whatever the method (the trailing-slash redirect asks this).
pub fn matches(path: &str) -> bool {
  matches!(path, REPLY_PATH | READ_PATH | CHECK_PATH)
}

/// `POST /api/internal/discord/reply`: checks the thread is fresh, then posts the text to it.
async fn post_reply(State(bot): State<Bot>, request: Request<Body>) -> Response {
  let features = Arc::clone(&bot.features);
  channel_service::post_reply_response(
    &features,
    request,
    "discord reply",
    move |session_id, text, request_id, run_started_at| async move {
      bot.post_reply_with_caller_context(&session_id, &text, request_id.as_deref(), run_started_at.as_deref()).await
    },
  )
  .await
}

/// `POST /api/internal/discord/read`: the session's own thread, or the channel the link names.
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
  let url = fields.get("url").and_then(Value::as_str).map(str::to_string);
  let limit = fields.get(LIMIT).and_then(Value::as_u64).unwrap_or(READ_LIMIT_DEFAULT as u64) as usize;
  reply_response(bot.read_thread(&session_id, url.as_deref(), limit).await)
}

/// `POST /api/internal/discord/check`: the bot token's setup.
async fn post_check(State(bot): State<Bot>, request: Request<Body>) -> Response {
  channel_service::check_response(request, bot.check_setup()).await
}
