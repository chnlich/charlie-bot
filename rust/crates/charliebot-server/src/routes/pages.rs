//! `GET /`: the chat page (`src/app/pages.py` `index`).
//!
//! The page renders `index.html` with the config's backend facts, the auth flag, the host name, the
//! version stamps and the voice dropdown's globals. The session half of the context (the sidebar
//! listing, the redirect to the first session, the `?session=` bootstrap) ports with the session
//! routes; until then the page renders the empty-sessions context, and a home whose sessions
//! directory holds anything fails the request rather than render a sidebar that omits its sessions.

use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::response::Response;
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::credentials::Credentials;
use charliebot_core::{paths, registry};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{Family, get_only};
use crate::responses::{self, Failure};
use crate::state::AppState;

pub const FAMILY: Family = Family { routes, matches: |_, path| path == "/" };

fn routes() -> Router<Arc<AppState>> {
  Router::new().route("/", get_only(index))
}

async fn index(State(state): State<Arc<AppState>>) -> Response {
  let rendered =
    tokio::task::spawn_blocking(move || render_index(&state)).await.expect("rendering the index does not panic");
  match rendered {
    Ok(html) => responses::html(html),
    Err(failure) => responses::internal_error(&failure.kind, failure.detail),
  }
}

fn failure(kind: &str, detail: impl ToString) -> Failure {
  Failure { kind: kind.to_string(), detail: detail.to_string() }
}

fn render_index(state: &AppState) -> Result<String, Failure> {
  let cfg = state.config().map_err(|error| failure("ValueError", error))?;
  let credentials = state.credentials().map_err(|error| failure("ValueError", error))?;
  require_no_sessions(&cfg.sessions_dir())?;
  let voice = VoiceSettings::from_config(&cfg).map_err(|error| failure("ValidationError", error))?;
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
    "backend_compact": compact_facts(&cfg)?,
    "default_backend_id": default_backend_id,
    "active_backend": default_backend_id,
    "active_backend_label": active_option.map_or(default_backend_id, |option| option.label.as_str()),
    "active_backend_type": active_option.map_or("", |option| option.backend_type.as_str()),
    "load_errors": [],
    "auth_enabled": credentials.access_key().is_some(),
    "hostname": hostname,
    "sessions_root": paths::display(&cfg.sessions_dir()),
    "version": state.templating.git_version(),
    "static_asset_version": state.templating.static_asset_version().map_err(|error| failure("OSError", error))?,
  });
  let globals = vec![
    ("voice_backends", minijinja::Value::from_serialize(voice.backends(&credentials))),
    ("voice_default_backend", minijinja::Value::from(voice.default_backend.clone())),
  ];
  state.templating.render("index.html", &context, globals).map_err(|error| failure(error.kind(), &error))
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
fn compact_facts(cfg: &CharlieBotConfig) -> Result<Map<String, Value>, Failure> {
  let mut facts = Map::new();
  for option in &cfg.backends.options {
    let traits = registry::backend_type(&option.backend_type)
      .ok_or_else(|| failure("KeyError", format!("backend type {:?} is not registered", option.backend_type)))?;
    facts.insert(option.backend_type.clone(), json!({"manual": traits.manual_compact, "hint": traits.compact_hint}));
  }
  Ok(facts)
}

/// `voice.engine`: the local transcription engine.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum VoiceEngine {
  Sherpa,
  Qwen3Hf,
}

impl VoiceEngine {
  fn name(self) -> &'static str {
    match self {
      VoiceEngine::Sherpa => "sherpa",
      VoiceEngine::Qwen3Hf => "qwen3_hf",
    }
  }
}

/// The transcription registry's backend ids, in dropdown order.
const TRANSCRIPTION_BACKENDS: &[&str] = &["local", "gemini", "muse"];

/// The two `voice:` keys the page reads, with `VoiceConfig`'s defaults. The voice package's port
/// types the whole section; this reads only what the dropdown shows.
#[derive(Debug, Clone, Deserialize)]
pub struct VoiceSettings {
  #[serde(default = "default_engine")]
  engine: VoiceEngine,
  #[serde(default = "default_transcription_backend")]
  default_backend: String,
}

fn default_engine() -> VoiceEngine {
  VoiceEngine::Sherpa
}

fn default_transcription_backend() -> String {
  "local".to_string()
}

impl VoiceSettings {
  /// The settings of `cfg`'s `voice:` section; an absent section takes every default.
  pub fn from_config(cfg: &CharlieBotConfig) -> Result<VoiceSettings, String> {
    let settings: VoiceSettings = match cfg.section("voice") {
      None | Some(serde_yaml::Value::Null) => {
        VoiceSettings { engine: default_engine(), default_backend: default_transcription_backend() }
      }
      Some(section) => serde_yaml::from_value(section.clone()).map_err(|error| format!("voice: {error}"))?,
    };
    if !TRANSCRIPTION_BACKENDS.contains(&settings.default_backend.as_str()) {
      return Err(format!(
        "voice.default_backend '{}' is not a transcription backend; known: {}",
        settings.default_backend,
        TRANSCRIPTION_BACKENDS.join(", ")
      ));
    }
    Ok(settings)
  }

  /// `page_globals.voice_backends()`: id, label, live partials and why the backend cannot run now.
  fn backends(&self, credentials: &Credentials) -> Value {
    let needs = |section: &str, key: &str| {
      let present = credentials.get(section, key).is_some_and(|value| value.is_truthy());
      if present { Value::Null } else { Value::from(format!("needs {section}.{key}")) }
    };
    json!([
      {"id": "local", "label": format!("Local ({})", self.engine.name()), "live_partials": false, "unavailable_reason": null},
      {"id": "gemini", "label": "Gemini 3.5 Transcribe Live", "live_partials": true, "unavailable_reason": needs("gemini", "api_key")},
      {"id": "muse", "label": "Muse Voice Transcribe", "live_partials": true, "unavailable_reason": needs("meta", "model_api_key")},
    ])
  }
}
