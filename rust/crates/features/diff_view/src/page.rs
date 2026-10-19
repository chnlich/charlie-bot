//! The diff viewer page (`src/features/diff_view/page.py`): `GET /diff`.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::request::Parts;
use axum::response::Response;
use charliebot_compat::responses;
use charliebot_compat::routing::get_only;
use charliebot_registry::FeatureContext;
use charliebot_registry::templating::Templating;
use serde_json::json;

/// The page's router, with the feature context as its state.
pub(crate) fn routes(features: &Arc<FeatureContext>) -> Router {
  Router::new().route("/diff", get_only(diff_viewer)).with_state(Arc::clone(features))
}

pub(crate) fn matches(path: &str) -> bool {
  path == "/diff"
}

/// `diff_viewer`: `diff.html` with the host name and the static asset version.
async fn diff_viewer(State(features): State<Arc<FeatureContext>>, parts: Parts) -> Response {
  let templating = match features.startup.require::<Templating>() {
    Ok(templating) => templating,
    Err(error) => return responses::internal_error("StateError", error.to_string()),
  };
  let static_asset_version = match templating.static_asset_version() {
    Ok(version) => version,
    Err(error) => return responses::internal_error("OSError", error.to_string()),
  };
  let hostname = match hostname() {
    Ok(hostname) => hostname,
    Err(response) => return response,
  };
  let context = json!({"hostname": hostname, "static_asset_version": static_asset_version});
  match templating.render_page(&features, "diff.html", &context, &parts) {
    Ok(response) => response,
    Err(error) => responses::internal_error(error.kind(), error.to_string()),
  }
}

/// `socket.gethostname()`.
fn hostname() -> Result<String, Response> {
  let name = nix::unistd::gethostname().map_err(|error| responses::internal_error("OSError", error.to_string()))?;
  name.into_string().map_err(|name| {
    responses::internal_error("UnicodeDecodeError", format!("host name is not UTF-8: {}", name.to_string_lossy()))
  })
}
