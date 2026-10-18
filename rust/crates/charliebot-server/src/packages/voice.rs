//! `src/features/voice`: the `voice:` section, the dropdown's template globals and the default-backend config check.
//! The transcription engines are not ported; this carries the two settings the chat page reads.

use std::sync::Arc;

use charliebot_core::config::{CharlieBotConfig, ConfigSection, SectionValue};
use charliebot_core::credentials::Credentials;
use charliebot_registry::{FeatureContext, Registry, RegistryError};
use minijinja::Value;
use serde::Deserialize;
use serde_json::json;

/// The transcription registry's backend ids, in dropdown order.
const TRANSCRIPTION_BACKENDS: &[&str] = &["local", "gemini", "muse"];

pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.config_section(ConfigSection {
    key: "voice",
    legacy_keys: &[("voice_engine", "voice.engine"), ("voice_model_id", "voice.model_id")],
    parse: parse_settings,
  })?;
  registry.template_global("voice_backends", voice_backends)?;
  registry.template_global("voice_default_backend", voice_default_backend)?;
  registry.config_check("check_default_backend", check_default_backend)
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

/// The two `voice:` keys the chat page reads, with `VoiceConfig`'s defaults.
#[derive(Debug, Clone, Deserialize)]
struct VoiceSettings {
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
  /// `page_globals.voice_backends()`: id, label, live partials and why the backend cannot run now.
  fn backends(&self, credentials: &Credentials) -> serde_json::Value {
    let needs = |section: &str, key: &str| {
      let present = credentials.get(section, key).is_some_and(|value| value.is_truthy());
      if present { serde_json::Value::Null } else { json!(format!("needs {section}.{key}")) }
    };
    json!([
      {"id": "local", "label": format!("Local ({})", self.engine.name()), "live_partials": false, "unavailable_reason": null},
      {"id": "gemini", "label": "Gemini 3.5 Transcribe Live", "live_partials": true, "unavailable_reason": needs("gemini", "api_key")},
      {"id": "muse", "label": "Muse Voice Transcribe", "live_partials": true, "unavailable_reason": needs("meta", "model_api_key")},
    ])
  }
}

/// The `voice:` section; an absent or empty section takes every default.
fn parse_settings(raw: &serde_yaml::Value) -> Result<SectionValue, String> {
  let settings: VoiceSettings = match raw {
    serde_yaml::Value::Null => {
      VoiceSettings { engine: default_engine(), default_backend: default_transcription_backend() }
    }
    _ => serde_yaml::from_value(raw.clone()).map_err(|error| format!("voice: {error}"))?,
  };
  Ok(Arc::new(settings))
}

/// `check_default_backend`: the default transcription backend must be one of the known ones.
fn check_default_backend(cfg: &CharlieBotConfig) -> Result<(), String> {
  let settings = cfg.section::<VoiceSettings>("voice");
  if TRANSCRIPTION_BACKENDS.contains(&settings.default_backend.as_str()) {
    return Ok(());
  }
  Err(format!(
    "voice.default_backend '{}' is not a transcription backend; known: {}",
    settings.default_backend,
    TRANSCRIPTION_BACKENDS.join(", ")
  ))
}

fn voice_backends(features: &FeatureContext) -> Result<Value, String> {
  let cfg = features.config.get().map_err(|error| error.to_string())?;
  let credentials = features.credentials.get().map_err(|error| error.to_string())?;
  Ok(Value::from_serialize(cfg.section::<VoiceSettings>("voice").backends(&credentials)))
}

fn voice_default_backend(features: &FeatureContext) -> Result<Value, String> {
  let cfg = features.config.get().map_err(|error| error.to_string())?;
  Ok(Value::from(cfg.section::<VoiceSettings>("voice").default_backend.clone()))
}
