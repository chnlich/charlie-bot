use std::collections::BTreeMap;
use std::sync::Arc;

use charliebot_core::config::{CREDENTIAL_KEY, ConfigSection, SectionValue};
use charliebot_core::credentials::Credentials;
use charliebot_core::text_repr;
use charliebot_registry::FeatureContext;
use minijinja::Value;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::transcription::{self, BackendEntry, CredentialRef};

/// The `voice` section. Each `voice.backends` entry in it is complete and names a transcription protocol.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(try_from = "RawVoiceSettings")]
pub(crate) struct VoiceSettings {
  /// The id a request names none of; unset, such a request fails with the key named.
  pub(crate) default_backend: Option<String>,
  /// Words the recognizer should favor, such as names and product terms. Gemini receives them as its custom vocabulary,
  /// and Muse as keywords. An empty list sends no words.
  pub(crate) vocabulary: Vec<String>,
  /// The transcript languages, by code. Only `zh` (Mandarin Chinese) and `en` (English) map to a protocol language.
  /// Any other code is dropped without a message, and the service then gets no language hint for it.
  pub(crate) languages: Vec<String>,
  /// One entry per backend id, `gemini` or `muse`; an entry's fields are described on [`BackendEntry`]. A backend with
  /// no entry stays in the voice menu as unavailable, and its reason names the `voice.backends` key to set. An entry
  /// that lacks a field fails the section's parse, and the error names that field.
  pub(crate) backends: BTreeMap<String, BackendEntry>,
}

/// The section as the file writes it. `try_from` checks it into `VoiceSettings`, so a loaded section is whole.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawVoiceSettings {
  /// The `voice.default_backend` key; its effect is on [`VoiceSettings::default_backend`].
  default_backend: Option<String>,
  /// The `voice.vocabulary` key; its effect is on [`VoiceSettings::vocabulary`].
  #[serde(default)]
  vocabulary: Vec<String>,
  /// The `voice.languages` key; its effect is on [`VoiceSettings::languages`].
  #[serde(default)]
  languages: Vec<String>,
  /// The `voice.backends` key; its effect is on [`VoiceSettings::backends`].
  #[serde(default)]
  backends: BTreeMap<String, RawBackendEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBackendEntry {
  /// The `voice.backends.<id>.label` key; its effect is on [`BackendEntry::label`].
  label: Option<String>,
  /// The `voice.backends.<id>.model` key; its effect is on [`BackendEntry::model`].
  model: Option<String>,
  /// The `voice.backends.<id>.endpoint` key; its effect is on [`BackendEntry::endpoint`].
  endpoint: Option<String>,
  /// The `voice.backends.<id>.credential` key; its effect is on [`BackendEntry::credential`].
  credential: Option<String>,
}

impl TryFrom<RawVoiceSettings> for VoiceSettings {
  type Error = String;

  fn try_from(raw: RawVoiceSettings) -> Result<Self, String> {
    if let Some(id) = raw.default_backend.as_deref().filter(|id| transcription::protocol_spec(id).is_none()) {
      return Err(format!(
        "voice.default_backend {} is not a transcription backend; known: {}",
        text_repr::str_repr(id),
        transcription::known_ids()
      ));
    }
    let backends = raw
      .backends
      .into_iter()
      .map(|(id, entry)| backend_entry(&id, entry).map(|entry| (id, entry)))
      .collect::<Result<_, _>>()?;
    Ok(Self { default_backend: raw.default_backend, vocabulary: raw.vocabulary, languages: raw.languages, backends })
  }
}

/// The entry a `voice.backends` key holds, once the key names a protocol and every field is set.
fn backend_entry(id: &str, raw: RawBackendEntry) -> Result<BackendEntry, String> {
  let protocol = transcription::protocol_spec(id).ok_or_else(|| {
    format!("voice.backends.{id} is not a transcription backend; known: {}", transcription::known_ids())
  })?;
  let required = |field: &str, value: Option<String>| {
    value.ok_or_else(|| format!("voice.backends.{id}.{field} is not set in config.yaml"))
  };
  let label = required("label", raw.label)?;
  let model = required("model", raw.model)?;
  let endpoint = required("endpoint", raw.endpoint)?;
  let credential = required(CREDENTIAL_KEY, raw.credential)?;
  let credential = CredentialRef::parse(&credential).ok_or_else(|| {
    format!("voice.backends.{id}.credential must be section.field, got {}", text_repr::str_repr(&credential))
  })?;
  Ok(BackendEntry { label, protocol, model, endpoint, credential })
}

impl VoiceSettings {
  /// The entry a backend id names. An id the config does not list fails with its key named.
  pub(crate) fn backend(&self, id: &str) -> Result<&BackendEntry, String> {
    self.backends.get(id).ok_or_else(|| format!("voice.backends.{id} is not configured in config.yaml"))
  }

  /// The id a request names none of. Unset, the request fails with the key named.
  pub(crate) fn default_backend_id(&self) -> Result<&str, String> {
    self.default_backend.as_deref().ok_or_else(|| "voice.default_backend is not set in config.yaml".to_string())
  }

  /// One row per protocol in catalog order, the rows the chat page's voice menu reads. A protocol with no entry is
  /// listed unavailable, and its reason names the key the host must set.
  fn backend_rows(&self, credentials: &Credentials) -> serde_json::Value {
    serde_json::Value::Array(
      transcription::BACKENDS
        .iter()
        .map(|spec| match self.backends.get(spec.id) {
          Some(entry) => json!({
            "id": spec.id,
            "label": entry.label,
            "live_partials": spec.live_partials,
            "unavailable_reason": transcription::unavailable_reason(entry, credentials),
          }),
          None => json!({
            "id": spec.id,
            "label": "",
            "live_partials": spec.live_partials,
            "unavailable_reason": format!("voice.backends.{} is not configured in config.yaml", spec.id),
          }),
        })
        .collect(),
    )
  }
}

pub(crate) fn section() -> ConfigSection {
  ConfigSection { key: "voice", legacy_keys: &[], parse: parse_settings, dump: dump_settings }
}

/// The section as its model dumps it (`charliebot config get voice`).
fn dump_settings(value: &SectionValue) -> Result<serde_json::Value, String> {
  let settings = value.downcast_ref::<VoiceSettings>().ok_or("the voice section holds an unexpected value")?;
  serde_json::to_value(settings).map_err(|error| error.to_string())
}

fn parse_settings(raw: &serde_yaml::Value) -> Result<SectionValue, String> {
  let settings: VoiceSettings = serde_yaml::from_value(raw.clone()).map_err(|error| format!("voice: {error}"))?;
  Ok(Arc::new(settings))
}

pub(crate) fn voice_backends(features: &FeatureContext) -> Result<Value, String> {
  let cfg = features.config.get().map_err(|error| error.to_string())?;
  let credentials = features.credentials.get().map_err(|error| error.to_string())?;
  Ok(Value::from_serialize(cfg.section::<VoiceSettings>("voice").backend_rows(&credentials)))
}

pub(crate) fn voice_default_backend(features: &FeatureContext) -> Result<Value, String> {
  let cfg = features.config.get().map_err(|error| error.to_string())?;
  Ok(Value::from_serialize(&cfg.section::<VoiceSettings>("voice").default_backend))
}

#[cfg(test)]
mod tests {
  use super::*;

  const GEMINI_ENTRY: &str = "    label: Synthetic Gemini
    model: models/synthetic-live
    endpoint: wss://synthetic.invalid/gemini
    credential: gemini.api_key
";

  const MUSE_ENTRY: &str = "    label: Synthetic Muse
    model: synthetic-muse
    endpoint: wss://synthetic.invalid/muse
    credential: meta.model_api_key
";

  #[test]
  fn an_unset_backend_key_fails_the_load_and_names_the_key() {
    let without_model = GEMINI_ENTRY.replace("    model: models/synthetic-live\n", "");
    let error =
      serde_yaml::from_str::<VoiceSettings>(&format!("default_backend: gemini\nbackends:\n  gemini:\n{without_model}"))
        .expect_err("an entry without its model must fail the load");
    assert!(error.to_string().contains("voice.backends.gemini.model is not set"), "{error}");
  }

  #[test]
  fn a_backend_with_no_entry_fails_at_use_and_names_its_key() {
    let settings: VoiceSettings = serde_yaml::from_str("default_backend: muse\n").expect("the section parses");
    assert_eq!(settings.backend("muse").unwrap_err(), "voice.backends.muse is not configured in config.yaml");
    assert_eq!(settings.default_backend_id(), Ok("muse"));
    let unset: VoiceSettings = serde_yaml::from_str("vocabulary: [Ada]\n").expect("the section parses");
    assert_eq!(unset.default_backend_id().unwrap_err(), "voice.default_backend is not set in config.yaml");
  }

  #[test]
  fn default_backend_and_backend_keys_must_name_a_protocol() {
    let error = serde_yaml::from_str::<VoiceSettings>("default_backend: \"unknown\\\\backend\"\n")
      .expect_err("an unknown default must fail the load");
    assert!(
      error
        .to_string()
        .contains("voice.default_backend 'unknown\\\\backend' is not a transcription backend; known: gemini, muse"),
      "{error}"
    );
    let error = serde_yaml::from_str::<VoiceSettings>(&format!("backends:\n  whisper:\n{GEMINI_ENTRY}"))
      .expect_err("an entry under an unknown key must fail the load");
    assert!(error.to_string().contains("voice.backends.whisper is not a transcription backend"), "{error}");
  }

  #[test]
  fn backend_rows_list_every_protocol_with_its_credential_or_key() {
    let home = tempfile::tempdir().expect("create a synthetic credentials home");
    std::fs::write(home.path().join("credentials.yaml"), "gemini:\n  api_key: synthetic-key\n")
      .expect("write synthetic credentials");
    let credentials = charliebot_core::credentials::load_credentials(home.path()).expect("load credentials");

    let both = format!("backends:\n  gemini:\n{GEMINI_ENTRY}  muse:\n{MUSE_ENTRY}");
    let settings: VoiceSettings = serde_yaml::from_str(&both).expect("both entries parse");
    let rows = settings.backend_rows(&credentials);
    assert_eq!(rows[0]["id"], "gemini");
    assert_eq!(rows[0]["label"], "Synthetic Gemini");
    assert_eq!(rows[0]["unavailable_reason"], serde_json::Value::Null);
    assert_eq!(rows[1]["id"], "muse");
    assert_eq!(rows[1]["unavailable_reason"], "needs meta.model_api_key");

    let gemini_only: VoiceSettings =
      serde_yaml::from_str(&format!("backends:\n  gemini:\n{GEMINI_ENTRY}")).expect("one entry parses");
    let rows = gemini_only.backend_rows(&credentials);
    assert_eq!(rows[1]["label"], "");
    assert_eq!(rows[1]["unavailable_reason"], "voice.backends.muse is not configured in config.yaml");
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
