//! `GET /`: the chat page (`src/app/pages.py` `index`).
//!
//! The page renders `index.html` with the config's backend facts, the auth flag, the host name, the version stamps and
//! the template globals the registered packages add. The session half of the context (the sidebar listing, the redirect
//! to the first session, the `?session=` bootstrap) ports with the session routes; until then the page renders the
//! empty-sessions context, and a home whose sessions directory holds anything fails the request rather than render a
//! sidebar that omits its sessions.

use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::request::Parts;
use axum::response::Response;
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::paths;
use charliebot_registry::Registry;
use charliebot_registry::templating::Templating;
use serde_json::{Map, Value, json};

use super::{Family, get_only};
use crate::state::AppState;
use charliebot_compat::responses::{self, Failure};

pub const FAMILY: Family = Family { routes, matches: |_, path| path == "/" };

fn routes() -> Router<Arc<AppState>> {
  Router::new().route("/", get_only(index))
}

async fn index(State(state): State<Arc<AppState>>, request: Parts) -> Response {
  let rendered = tokio::task::spawn_blocking(move || render_index(&state, &request))
    .await
    .expect("rendering the index does not panic");
  match rendered {
    Ok(response) => response,
    Err(failure) => responses::internal_error(&failure.kind, failure.detail),
  }
}

fn failure(kind: &str, detail: impl ToString) -> Failure {
  Failure { kind: kind.to_string(), detail: detail.to_string() }
}

fn render_index(state: &AppState, request: &Parts) -> Result<Response, Failure> {
  let cfg = state.config().map_err(|error| failure("ValueError", error))?;
  let credentials = state.credentials().map_err(|error| failure("ValueError", error))?;
  require_no_sessions(&cfg.sessions_dir())?;
  let templating =
    state.features.startup.require::<Templating>().expect("run() inserts the Templating before the server binds");
  let options = &cfg.backends.options;
  let default_backend_id = options.first().map_or("claude", |option| option.id.as_str());
  let active_option = cfg.get_backend_option(default_backend_id);
  let hostname = nix::unistd::gethostname()
    .map_err(|error| failure("OSError", error))?
    .into_string()
    .map_err(|name| failure("UnicodeDecodeError", format!("host name is not UTF-8: {}", name.to_string_lossy())))?;
  let context = json!({
    "initial_sessions": [],
    "active_session": null,
    "pending_draft": null,
    "event_count": 0,
    "session_bootstrap": null,
    "backend_options": options
      .iter()
      .map(|option| json!({"id": option.id, "label": option.label, "type": option.backend_type}))
      .collect::<Vec<_>>(),
    "backend_compact": compact_facts(&cfg, &state.features.registry)?,
    "default_backend_id": default_backend_id,
    "active_backend": default_backend_id,
    "active_backend_label": active_option.map_or(default_backend_id, |option| option.label.as_str()),
    "active_backend_type": active_option.map_or("", |option| option.backend_type.as_str()),
    "load_errors": [],
    "auth_enabled": credentials.access_key().is_some(),
    "hostname": hostname,
    "sessions_root": paths::display(&cfg.sessions_dir()),
    "version": templating.git_version(),
    "static_asset_version": templating.static_asset_version().map_err(|error| failure("OSError", error))?,
  });
  templating
    .render_page(&state.features, "index.html", &context, request)
    .map_err(|error| failure(error.kind(), &error))
}

/// The session listing is not ported: a home with anything under `sessions/` fails the request.
fn require_no_sessions(sessions_dir: &Path) -> Result<(), Failure> {
  let mut entries = match std::fs::read_dir(sessions_dir) {
    Ok(entries) => entries,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
    Err(error) => return Err(failure("OSError", format!("{}: {error}", sessions_dir.display()))),
  };
  if entries.next().is_some() {
    return Err(failure(
      "NotImplementedError",
      format!("{} holds sessions, and the session sidebar is not ported", sessions_dir.display()),
    ));
  }
  Ok(())
}

/// `_compact_facts`: per configured backend type, whether manual compaction runs and the hint.
fn compact_facts(cfg: &CharlieBotConfig, registry: &Registry) -> Result<Map<String, Value>, Failure> {
  let mut facts = Map::new();
  for option in &cfg.backends.options {
    let registration = registry
      .backend_type_of(&option.backend_type)
      .ok_or_else(|| failure("KeyError", format!("backend type {:?} is not registered", option.backend_type)))?;
    let traits = registration.traits;
    facts.insert(option.backend_type.clone(), json!({"manual": traits.manual_compact, "hint": traits.compact_hint}));
  }
  Ok(facts)
}
