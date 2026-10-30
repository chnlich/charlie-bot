use std::sync::Arc;

use charliebot_core::config::{CharlieBotConfig, ConfigSection, SectionValue};
use charliebot_core::credentials::Credentials;
use charliebot_core::text_repr;
use charliebot_registry::FeatureContext;
use minijinja::Value;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::recording;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VoiceSettings {
  #[serde(default = "default_backend")]
  pub(crate) default_backend: String,
  #[serde(default)]
  pub(crate) vocabulary: Vec<String>,
  #[serde(default)]
  pub(crate) languages: Vec<String>,
}

impl Default for VoiceSettings {
  fn default() -> Self {
    Self { default_backend: default_backend(), vocabulary: Vec::new(), languages: Vec::new() }
  }
}

fn default_backend() -> String {
  "gemini".to_string()
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
  ConfigSection { key: "voice", legacy_keys: &[], parse: parse_settings, dump: dump_settings }
}

/// Render the section as its model dumps it (`charliebot config get voice`).
fn dump_settings(value: &SectionValue) -> Result<serde_json::Value, String> {
  let settings = value.downcast_ref::<VoiceSettings>().ok_or("the voice section holds an unexpected value")?;
  serde_json::to_value(settings).map_err(|error| error.to_string())
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
  if recording::BACKEND_IDS.contains(&value.as_str()) {
    return Ok(());
  }
  Err(format!(
    "voice.default_backend {} is not a transcription backend; known: {}",
    text_repr::str_repr(value),
    recording::BACKEND_IDS.join(", ")
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
  fn config_defaults_and_custom_values_cover_the_three_voice_fields() {
    let default = VoiceSettings::default();
    assert_eq!(default.default_backend, "gemini");
    assert!(default.vocabulary.is_empty());
    assert!(default.languages.is_empty());

    let custom: VoiceSettings = serde_yaml::from_str("default_backend: muse\nvocabulary: [Ada]\nlanguages: [en]\n")
      .expect("all voice fields parse");
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
    assert_eq!(backends[0]["id"], "gemini");
    assert_eq!(backends[0]["unavailable_reason"], serde_json::Value::Null);
    assert_eq!(backends[1]["id"], "muse");
    assert_eq!(backends[1]["unavailable_reason"], "needs meta.model_api_key");
  }

  #[test]
  fn default_backend_check_matches_python_backend_ids() {
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
    assert!(registry.config_schema().legacy_keys.is_empty());
    let cfg = charliebot_core::config::load_config(home.path(), &registry.config_schema())
      .expect("parse config before its registered check");
    assert_eq!(
      check_default_backend(&cfg),
      Err("voice.default_backend 'unknown\\\\backend' is not a transcription backend; known: gemini, muse".to_string())
    );
  }

  #[test]
  fn removed_engine_fields_are_unknown_voice_keys() {
    for field in ["engine", "model_id"] {
      let error = serde_yaml::from_str::<VoiceSettings>(&format!("{field}: synthetic\n"))
        .expect_err("the removed local-engine setting must fail config loading");
      assert!(error.to_string().contains("unknown field"), "{error}");
    }
  }
}
