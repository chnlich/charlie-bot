//! The `/static` mounts: each page package's `static/`, then `web/static`.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::response::Response;
use axum::routing::any;

use super::Family;
use crate::state::AppState;
use crate::{request_path, static_files};

pub const FAMILY: Family =
  Family { routes, matches: |state, path| state.static_mounts.iter().any(|mount| mount.route_path(path).is_some()) };

fn routes() -> Router<Arc<AppState>> {
  Router::new().route("/static/", any(serve)).route("/static/{*rest}", any(serve))
}

/// The first mount whose prefix holds the path serves it; a path under no mount (no `web/static` at
/// startup) takes the router's not-found answer.
async fn serve(State(state): State<Arc<AppState>>, request: Request) -> Response {
  let path = request_path::of(&request).to_string();
  let Some(index) = state.static_mounts.iter().position(|mount| mount.route_path(&path).is_some()) else {
    return super::not_found(State(state), request).await;
  };
  let method = request.method().clone();
  let headers = request.headers().clone();
  tokio::task::spawn_blocking(move || {
    let mount = &state.static_mounts[index];
    let below = mount.route_path(&path).expect("the mount matched this path above");
    static_files::serve(mount, &state.mime_types, below, &method, &headers)
  })
  .await
  .expect("serving a static file does not panic")
}
