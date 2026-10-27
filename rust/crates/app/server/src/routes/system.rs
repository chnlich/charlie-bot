//! `GET /api/auth/status` and `GET /api/internal/version`.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::response::Response;
use charliebot_compat::responses;
use charliebot_compat::routing::get_only;
use http::StatusCode;
use serde_json::json;

use super::family::Family;
use crate::auth::AUTH_STATUS_PATH;
use crate::state::AppState;

/// `internal.get_version`'s path: the CLI's version-skew probe.
pub const VERSION_PATH: &str = "/api/internal/version";

pub const FAMILY: Family = Family { routes, matches: |_, path| path == AUTH_STATUS_PATH || path == VERSION_PATH };

fn routes() -> Router<Arc<AppState>> {
  Router::new().route(AUTH_STATUS_PATH, get_only(auth_status)).route(VERSION_PATH, get_only(version))
}

/// `pages.auth_status`: whether access-key auth is on. A public path, so the login page can ask.
async fn auth_status(State(state): State<Arc<AppState>>) -> Response {
  match state.access_key() {
    Ok(key) => responses::json(StatusCode::OK, &json!({"auth_enabled": key.is_some()})),
    Err(error) => responses::internal_error("ValueError", error.to_string()),
  }
}

/// `internal.get_version`: the build sha and start time.
async fn version(State(state): State<Arc<AppState>>) -> Response {
  responses::json(StatusCode::OK, &state.build_info.to_json())
}
