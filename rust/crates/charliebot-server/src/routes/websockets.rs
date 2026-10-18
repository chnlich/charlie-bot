//! WebSocket upgrades: `/ws/sessions/{session_id}`, `/ws/terminal` and `/ws/voice/{session_id}`.
//!
//! uvicorn hands a request whose `Connection` tokens include `upgrade` and whose `Upgrade` is
//! `websocket` to its WebSocket protocol, outside the HTTP middleware: no auth gate, no request log, no
//! gzip. An unknown path, or a route whose `token` query parameter fails `check_ws_auth`, closes before
//! accepting, which the protocol answers as a bare 403. The streams behind the three routes are not
//! ported yet, so an authorized upgrade answers the same 403 and logs `websocket_route_not_ported`.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use http::{HeaderMap, StatusCode, header};

use crate::auth;
use crate::request_path;
use crate::state::AppState;

/// The WebSocket route patterns, `*` standing for one path segment.
const ROUTES: &[&str] = &["/ws/sessions/*", "/ws/terminal", "/ws/voice/*"];

/// uvicorn's `_get_upgrade`: the last `Upgrade` value, lowercased, when the last `Connection` header's
/// comma-separated tokens include `upgrade`.
pub fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
  let last_text = |name: header::HeaderName| {
    headers.get_all(name).iter().next_back().map(|value| value.as_bytes().to_ascii_lowercase())
  };
  let connection_upgrade = last_text(header::CONNECTION)
    .is_some_and(|value| value.split(|byte| *byte == b',').any(|token| token.trim_ascii() == b"upgrade"));
  connection_upgrade && last_text(header::UPGRADE).is_some_and(|value| value == b"websocket")
}

/// Whether a decoded path matches a WebSocket route.
fn matches_route(path: &str) -> bool {
  let segments: Vec<&str> = path.split('/').collect();
  ROUTES.iter().any(|route| {
    let pattern: Vec<&str> = route.split('/').collect();
    pattern.len() == segments.len()
      && pattern.iter().zip(&segments).all(|(want, got)| if *want == "*" { !got.is_empty() } else { want == got })
  })
}

/// The gate in front of the HTTP stack: upgrades end here.
pub async fn gate(State(state): State<Arc<AppState>>, request: Request, next: Next) -> Response {
  if !is_websocket_upgrade(request.headers()) {
    return next.run(request).await;
  }
  let path = request_path::of(&request);
  if matches_route(path) {
    let key = match state.access_key() {
      Ok(key) => key,
      Err(error) => {
        tracing::error!(error = %error, "Exception in ASGI application");
        return forbidden();
      }
    };
    match auth::websocket_authorized(request.uri().query(), key.as_deref()) {
      Ok(true) => tracing::warn!(path = %path, "websocket_route_not_ported"),
      Ok(false) => {}
      Err(auth::NonAsciiCredential) => {
        tracing::error!(error = "TypeError", "Exception in ASGI application");
      }
    }
  }
  forbidden()
}

/// The `websockets` protocol's answer to a close before accept.
fn forbidden() -> Response {
  Response::builder()
    .status(StatusCode::FORBIDDEN)
    .header(header::CONTENT_LENGTH, "0")
    .header(header::CONTENT_TYPE, "text/plain")
    .header(header::CONNECTION, "close")
    .body(Body::empty())
    .expect("a fixed 403 response builds")
}

#[cfg(test)]
mod tests {
  use super::*;
  use http::HeaderValue;

  #[test]
  fn upgrade_detection_follows_uvicorn() {
    let mut headers = HeaderMap::new();
    headers.insert(header::CONNECTION, HeaderValue::from_static("keep-alive, Upgrade"));
    headers.insert(header::UPGRADE, HeaderValue::from_static("WebSocket"));
    assert!(is_websocket_upgrade(&headers));
    headers.insert(header::UPGRADE, HeaderValue::from_static("h2c"));
    assert!(!is_websocket_upgrade(&headers));
    headers.remove(header::CONNECTION);
    headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    assert!(!is_websocket_upgrade(&headers));
  }

  #[test]
  fn routes_match_one_segment_per_parameter() {
    assert!(matches_route("/ws/sessions/s-1"));
    assert!(matches_route("/ws/terminal"));
    assert!(!matches_route("/ws/sessions/"));
    assert!(!matches_route("/ws/sessions/a/b"));
    assert!(!matches_route("/ws/terminal/"));
  }
}
