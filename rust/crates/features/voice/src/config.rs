use std::sync::Arc;

use charliebot_core::config::{CharlieBotConfig, ConfigSection, SectionValue};
use charliebot_core::credentials::Credentials;
use charliebot_core::pyfmt;
use charliebot_registry::FeatureContext;
use minijinja::Value;
use serde::Deserialize;
use serde_json::json;

pub(crate) const BACKEND_IDS: &[&str] = &["local", "gemini", "muse"];
const LEGACY_KEYS: &[(&str, &str)] = &[("voice_engine", "voice.engine"), ("voice_model_id", "voice.model_id")];

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum VoiceEngine {
  Sherpa,
  Qwen3Hf,
}

impl VoiceEngine {
  pub(crate) fn name(self) -> &'static str {
    match self {
      VoiceEngine::Sherpa => "sherpa",
      VoiceEngine::Qwen3Hf => "qwen3_hf",
    }
  }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VoiceSettings {
  #[serde(default = "default_engine")]
  pub(crate) engine: VoiceEngine,
  #[serde(default = "default_model_id")]
  #[allow(dead_code)]
  model_id: String,
  #[serde(default = "default_backend")]
  pub(crate) default_backend: String,
  #[serde(default)]
  pub(crate) vocabulary: Vec<String>,
  #[serde(default)]
  pub(crate) languages: Vec<String>,
}

impl Default for VoiceSettings {
  fn default() -> Self {
    Self {
      engine: default_engine(),
      model_id: default_model_id(),
      default_backend: default_backend(),
      vocabulary: Vec::new(),
      languages: Vec::new(),
    }
  }
}

fn default_engine() -> VoiceEngine {
  VoiceEngine::Sherpa
}

fn default_model_id() -> String {
  "Qwen/Qwen3-ASR-1.7B-hf".to_string()
}

fn default_backend() -> String {
  "local".to_string()
}

impl VoiceSettings {
  fn backends(&self, credentials: &Credentials) -> serde_json::Value {
    let availability = |section: &str, key: &str| {
      if credentials.get(section, key).is_some_and(|value| value.is_truthy()) {
        serde_json::Value::Null
      } else {
        json!(format!("needs {section}.{key}"))
      }
    };
    json!([
      {
        "id": "local",
        "label": format!("Local ({})", self.engine.name()),
        "live_partials": false,
        "unavailable_reason": null
      },
      {
        "id": "gemini",
        "label": "Gemini 3.5 Transcribe Live",
        "live_partials": true,
        "unavailable_reason": availability("gemini", "api_key")
      },
      {
        "id": "muse",
        "label": "Muse Voice Transcribe",
        "live_partials": true,
        "unavailable_reason": availability("meta", "model_api_key")
      }
    ])
  }
}

pub(crate) fn section() -> ConfigSection {
  ConfigSection { key: "voice", legacy_keys: LEGACY_KEYS, parse: parse_settings }
}

fn parse_settings(raw: &serde_yaml::Value) -> Result<SectionValue, String> {
  let settings = match raw {
    serde_yaml::Value::Null => VoiceSettings::default(),
    _ => serde_yaml::from_value(raw.clone()).map_err(|error| format!("voice: {error}"))?,
  };
  Ok(Arc::new(settings))
}

pub(crate) fn check_default_backend(cfg: &CharlieBotConfig) -> Result<(), String> {
  let value = &cfg.section::<VoiceSettings>("voice").default_backend;
  if BACKEND_IDS.contains(&value.as_str()) {
    return Ok(());
  }
  Err(format!(
    "voice.default_backend {} is not a transcription backend; known: {}",
    pyfmt::str_repr(value),
    BACKEND_IDS.join(", ")
  ))
}

pub(crate) fn voice_backends(features: &FeatureContext) -> Result<Value, String> {
  let cfg = features.config.get().map_err(|error| error.to_string())?;
  let credentials = features.credentials.get().map_err(|error| error.to_string())?;
  let settings = cfg.section::<VoiceSettings>("voice");
  Ok(Value::from_serialize(settings.backends(&credentials)))
}

pub(crate) fn voice_default_backend(features: &FeatureContext) -> Result<Value, String> {
  let cfg = features.config.get().map_err(|error| error.to_string())?;
  Ok(Value::from(cfg.section::<VoiceSettings>("voice").default_backend.clone()))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn config_defaults_and_custom_values_cover_all_five_fields() {
    let default = VoiceSettings::default();
    assert_eq!(default.engine.name(), "sherpa");
    assert_eq!(default.model_id, "Qwen/Qwen3-ASR-1.7B-hf");
    assert_eq!(default.default_backend, "local");
    assert!(default.vocabulary.is_empty());
    assert!(default.languages.is_empty());

    let custom: VoiceSettings = serde_yaml::from_str(
      "engine: qwen3_hf\nmodel_id: synthetic/model\ndefault_backend: muse\nvocabulary: [Ada]\nlanguages: [en]\n",
    )
    .expect("all voice fields parse");
    assert_eq!(custom.engine.name(), "qwen3_hf");
    assert_eq!(custom.model_id, "synthetic/model");
    assert_eq!(custom.default_backend, "muse");
    assert_eq!(custom.vocabulary, ["Ada"]);
    assert_eq!(custom.languages, ["en"]);
    assert!(serde_yaml::from_str::<VoiceSettings>("unexpected: true").is_err());
  }

  #[test]
  fn backend_dropdown_reports_credentials_in_registry_order() {
    let home = tempfile::tempdir().expect("create a synthetic credentials home");
    std::fs::write(home.path().join("credentials.yaml"), "gemini:\n  api_key: synthetic-key\n")
      .expect("write synthetic credentials");
    let credentials = charliebot_core::credentials::load_credentials(home.path()).expect("load credentials");
    let backends = VoiceSettings::default().backends(&credentials);
    assert_eq!(backends[0]["id"], "local");
    assert_eq!(backends[1]["unavailable_reason"], serde_json::Value::Null);
    assert_eq!(backends[2]["unavailable_reason"], "needs meta.model_api_key");
  }

  #[test]
  fn legacy_keys_and_default_backend_check_match_python_contract() {
    let home = tempfile::tempdir().expect("make an isolated config home");
    std::fs::write(
      home.path().join("config.yaml"),
      r#"voice:
  default_backend: "unknown\\backend"
"#,
    )
    .expect("write a synthetic config");
    let mut registry = charliebot_registry::Registry::new();
    registry.register_package("src.features.voice", crate::register).expect("register voice config");
    assert_eq!(registry.config_schema().legacy_keys, LEGACY_KEYS);
    let cfg = charliebot_core::config::load_config(home.path(), &registry.config_schema())
      .expect("parse config before its registered check");
    assert_eq!(
      check_default_backend(&cfg),
      Err(
        "voice.default_backend 'unknown\\\\backend' is not a transcription backend; known: local, gemini, muse"
          .to_string()
      )
    );
  }
}
