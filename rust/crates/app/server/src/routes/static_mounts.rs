//! The `/static` mounts: each page package's `static/`, then `web/static`.

use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::response::Response;
use axum::routing::any;
use charliebot_compat::static_files::{self, StaticDirectory};
use charliebot_registry::pages::package_directory;

use super::Family;
use crate::request_path;
use crate::state::AppState;

pub const FAMILY: Family =
  Family { routes, matches: |state, path| state.static_mounts.iter().any(|mount| mount.route_path(path).is_some()) };

/// One `app.mount(prefix, StaticFiles(directory=...))`.
pub struct StaticMount {
  /// The URL prefix without a trailing slash: `/static/features/voice` or `/static`.
  pub prefix: String,
  pub directory: StaticDirectory,
}

impl StaticMount {
  /// The Python app's `/static` mounts in its route order: one per page package in registration order, then
  /// `web/static` when it exists at startup.
  pub fn python_app_mounts(repo_root: &Path, packages: &[&str]) -> Vec<StaticMount> {
    let mut mounts: Vec<StaticMount> = packages
      .iter()
      .map(|package| StaticMount {
        prefix: format!("/static/{}", package.trim_start_matches("src.").replace('.', "/")),
        directory: StaticDirectory::new(package_directory(repo_root, package).join("static")),
      })
      .collect();
    let web_static = repo_root.join("web").join("static");
    if web_static.exists() {
      mounts.push(StaticMount { prefix: "/static".to_string(), directory: StaticDirectory::new(web_static) });
    }
    mounts
  }

  /// The path below this mount when `path` falls under it (starlette `Mount` matching), else `None`.
  pub fn route_path<'a>(&self, path: &'a str) -> Option<&'a str> {
    path.strip_prefix(&self.prefix).filter(|rest| rest.starts_with('/'))
  }
}

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
    static_files::serve(&mount.directory, &state.mime_types, below, &method, &headers)
  })
  .await
  .expect("serving a static file does not panic")
}
