//! The internal Discord routes behind `charliebot discord reply`, `read` and `check`.
//! Each body is validated before the handler runs, so a malformed request answers 422 whatever the session is. A
//! refusal of the handler answers its status with `{"detail": ...}`, and an unhandled failure answers 500.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::Request;
use axum::response::Response;
use charliebot_chat_threads::channel_service::reply_response;
use charliebot_cli_common::channel_verb::{READ_LIMIT_DEFAULT, READ_LIMIT_MAX, READ_LIMIT_MIN};
use charliebot_compat::request_body::{self, BodyModel, Field, FieldType, text_field};
use charliebot_compat::responses;
use charliebot_compat::routing::post_only;
use charliebot_compat::validation::Bound;
use charliebot_registry::FeatureContext;
use charliebot_registry::caller;
use serde_json::Value;

use crate::discord_client::BASE_URL;
use crate::discord_commands;
use crate::discord_listener::{self, Bot};

/// `DiscordReplyRequest`: the calling session's id and the reply text.
const REPLY_FIELDS: &[Field] =
  &[Field::required("session_id", FieldType::String), Field::required("text", FieldType::String)];
const REPLY_BODY: BodyModel = BodyModel::required(REPLY_FIELDS).forbid_extra();

/// `DiscordReadRequest`: the calling session's id, an optional channel link, and the window size.
const READ_FIELDS: &[Field] = &[
  Field::required("session_id", FieldType::String),
  Field::optional("url", FieldType::NullableString),
  Field::optional("limit", FieldType::Integer).ge(Bound::Int(READ_LIMIT_MIN)).le(Bound::Int(READ_LIMIT_MAX)),
];
const READ_BODY: BodyModel = BodyModel::required(READ_FIELDS).forbid_extra();

/// `DiscordCheckRequest`: no fields.
const CHECK_BODY: BodyModel = BodyModel::required(&[]).forbid_extra();

/// The three internal route paths. The `cli` verbs build their requests from these names, so a rename here cannot
/// leave a verb posting to a path the router no longer serves.
pub(crate) const REPLY_PATH: &str = "/api/internal/discord/reply";
pub(crate) const READ_PATH: &str = "/api/internal/discord/read";
pub(crate) const CHECK_PATH: &str = "/api/internal/discord/check";

/// The route table on Discord's REST base.
pub fn routes(features: &Arc<FeatureContext>) -> Router {
  routes_at(features, BASE_URL)
}

/// The route table with the REST base `base_url`; the tests point it at a loopback fake.
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
  let (parts, stream) = request.into_parts();
  let body = match request_body::read(&parts.headers, stream).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  let fields = match body.declared_fields(&REPLY_BODY) {
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
          "discord reply",
          &session_id,
          &text,
        )),
        Some(started_at),
      )
    }
    None => (None, None),
  };
  let result = discord_listener::post_reply_with_caller_context(
    &bot,
    &session_id,
    &text,
    request_id.as_deref(),
    run_started_at.as_deref(),
  )
  .await;
  reply_response(result)
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
  let session_id = text_field(&fields, "session_id");
  let url = fields.get("url").and_then(Value::as_str).map(str::to_string);
  let limit = fields.get("limit").and_then(Value::as_u64).unwrap_or(READ_LIMIT_DEFAULT as u64) as usize;
  reply_response(discord_commands::read_thread(&bot, &session_id, url.as_deref(), limit).await)
}

/// `POST /api/internal/discord/check`: the bot token's setup.
async fn post_check(State(bot): State<Bot>, request: Request<Body>) -> Response {
  let (parts, stream) = request.into_parts();
  let body = match request_body::read(&parts.headers, stream).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  if let Err(response) = body.declared_fields(&CHECK_BODY) {
    return *response;
  }
  reply_response(discord_commands::check_setup(&bot).await)
}
