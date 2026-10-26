//! The route families and the one list that mounts them.
//!
//! Each family is a module with its axum routes and a path matcher. The matcher answers whether a
//! path falls under one of the family's routes, which the trailing-slash redirect consults the way
//! starlette's router tries the slash-toggled path against every route. WebSocket upgrades never reach
//! these routes: [`websockets::gate`] answers them first.

pub mod pages;
pub mod static_mounts;
pub mod system;
pub mod websockets;

use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::response::Response;
use charliebot_compat::responses;
use http::{StatusCode, header};

use crate::app::ConnectionInfo;
use crate::request_path;
use crate::state::AppState;

/// One route family.
pub struct Family {
  /// The family's routes.
  pub routes: fn() -> Router<Arc<AppState>>,
  /// Whether a decoded HTTP path falls under one of the family's routes, whatever the method.
  pub matches: fn(&AppState, &str) -> bool,
}

/// Every route family, in the Python app's route order.
pub const FAMILIES: &[Family] = &[pages::FAMILY, system::FAMILY, static_mounts::FAMILY];

/// The HTTP router: every family's routes, then the not-found answer.
pub fn router() -> Router<Arc<AppState>> {
  FAMILIES.iter().fold(Router::new(), |router, family| router.merge((family.routes)())).fallback(not_found)
}

/// starlette's router when no route matches: a 307 to the slash-toggled path when that path matches
/// a route, else 404. The routes are the route families and the registered routers.
pub async fn not_found(State(state): State<Arc<AppState>>, request: Request) -> Response {
  let path = request_path::of(&request);
  if path != "/" {
    let toggled = if path.ends_with('/') { path.trim_end_matches('/').to_string() } else { format!("{path}/") };
    let family_matches = FAMILIES.iter().any(|family| (family.matches)(&state, &toggled));
    let router_matches = state.features.registry.routers().iter().any(|route| route.claims(&toggled));
    if family_matches || router_matches {
      return responses::redirect(&redirect_url(&request, &toggled));
    }
  }
  responses::http_exception(StatusCode::NOT_FOUND)
}

/// starlette `URL(scope=...)` with the path replaced: the `Host` header when present, else the
/// socket's own address; the query string carries over.
fn redirect_url(request: &Request, path: &str) -> String {
  let host =
    request.headers().get(header::HOST).map(|value| value.as_bytes().iter().map(|b| *b as char).collect::<String>());
  let mut url = match host {
    Some(host) => format!("http://{host}{path}"),
    None => {
      let server = request
        .extensions()
        .get::<axum::extract::ConnectInfo<ConnectionInfo>>()
        .expect("the server records every connection's addresses")
        .0
        .server;
      if server.port() == 80 {
        format!("http://{}{path}", server.ip())
      } else {
        format!("http://{}:{}{path}", server.ip(), server.port())
      }
    }
  };
  if let Some(query) = request.uri().query().filter(|query| !query.is_empty()) {
    url.push('?');
    url.push_str(query);
  }
  url
}
