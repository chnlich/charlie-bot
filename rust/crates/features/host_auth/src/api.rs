//! The host-auth routes (`src/features/host_auth/api.py`): the page, the status JSON and the manual probe.

use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::request::Parts;
use axum::response::Response;
use axum::routing::{get, post};
use charliebot_compat::responses;
use charliebot_registry::FeatureContext;
use http::{StatusCode, header};
use serde_json::{Value, json};
use time::OffsetDateTime;

use crate::host_auth::{self, RoundSlot};
use crate::page;
use crate::timestamps;

/// Register host-auth routes with the application feature context as shared state.
pub fn routes(features: &Arc<FeatureContext>) -> Router {
  Router::new()
    .route("/host-auth", get(host_auth_page).head(get_not_allowed).fallback(get_not_allowed))
    .route("/api/host-auth/status", get(host_auth_status).head(get_not_allowed).fallback(get_not_allowed))
    .route("/api/host-auth/probe", post(host_auth_probe).fallback(post_not_allowed))
    .with_state(Arc::clone(features))
}

/// The paths the routes answer, for the trailing-slash redirect: a slashed form of one of them redirects to it.
pub fn matches(path: &str) -> bool {
  matches!(path, "/host-auth" | "/api/host-auth/status" | "/api/host-auth/probe")
}

/// `GET /host-auth`: the standing page, rendered from the state file.
async fn host_auth_page(State(features): State<Arc<FeatureContext>>, request: Parts) -> Response {
  let rendered = tokio::task::spawn_blocking(move || {
    let state_path = host_auth::default_state_path().map_err(|error| ("Exception".to_string(), error.to_string()))?;
    let state = host_auth::load_state(&state_path);
    page::render(&features, &state, timestamps::utc_now(), &request)
  })
  .await
  .expect("rendering the host-auth page does not panic");
  match rendered {
    Ok(response) => response,
    Err((kind, detail)) => responses::internal_error(&kind, detail),
  }
}

/// `GET /api/host-auth/status`: the state file, each host with its derived estimate after its stored keys.
async fn host_auth_status() -> Response {
  let body = tokio::task::spawn_blocking(|| -> anyhow::Result<Value> {
    status_body(&host_auth::default_state_path()?, timestamps::utc_now())
  })
  .await
  .expect("reading the host-auth status does not panic");
  match body {
    Ok(value) => responses::json(StatusCode::OK, &value),
    Err(error) => failure(error),
  }
}

/// The status body: `{**state, "hosts": [{**entry, **estimate}]}`.
pub fn status_body(state_path: &Path, now: OffsetDateTime) -> anyhow::Result<Value> {
  let mut state = host_auth::load_state(state_path);
  let mut hosts = Vec::new();
  for entry in host_auth::host_entries(&state)? {
    let estimate = host_auth::derive_estimate(entry, now)?;
    let mut host = entry.clone();
    host.insert("estimated_expires_at".to_string(), json!(estimate.expires_at));
    host.insert("remaining_sec".to_string(), json!(estimate.remaining_sec));
    hosts.push(Value::Object(host));
  }
  state.insert("hosts".to_string(), Value::Array(hosts));
  Ok(Value::Object(state))
}

/// `POST /api/host-auth/probe`: start a forced round unless one runs, and return to the page either way.
async fn host_auth_probe() -> Response {
  if let Some(slot) = RoundSlot::claim() {
    tokio::spawn(manual_round(slot));
  }
  see_other()
}

/// The manual round: forced, with its failure logged. The slot is released when the round ends.
async fn manual_round(_slot: RoundSlot) {
  let outcome = async {
    let (state_path, config_path) = host_auth::default_paths()?;
    host_auth::run_round(true, &state_path, &config_path).await?;
    anyhow::Ok(())
  }
  .await;
  if let Err(error) = outcome {
    tracing::error!(error = %format!("{error:#}"), "host_auth_manual_round_failed");
  }
}

/// `RedirectResponse("/host-auth", status_code=303)`: an empty body, its length and its location.
fn see_other() -> Response {
  Response::builder()
    .status(StatusCode::SEE_OTHER)
    .header(header::CONTENT_LENGTH, "0")
    .header(header::LOCATION, "/host-auth")
    .body(Body::empty())
    .expect("the redirect builds")
}

async fn get_not_allowed() -> Response {
  responses::method_not_allowed("GET")
}

async fn post_not_allowed() -> Response {
  responses::method_not_allowed("POST")
}

/// `ServerErrorMiddleware`'s answer to a handler that raised.
fn failure(error: anyhow::Error) -> Response {
  responses::internal_error("Exception", format!("{error:#}"))
}
