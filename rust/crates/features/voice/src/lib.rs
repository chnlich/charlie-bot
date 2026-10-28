//! Voice input: the live preview relay, the online upload transcription and the turn contribution.

mod api;
mod archive;
mod audio;
mod config;
mod decode;
mod edit_list;
mod file;
mod matroska;
mod opus;
mod recording;
mod relay;
mod transcription;
mod wav;

pub use file::transcribe_file;

use std::sync::Arc;

use charliebot_registry::turn::TurnContribution;
use charliebot_registry::{Registry, RegistryError};
use charliebot_store::metadata::SessionMetadata;
use serde_json::Value;

const VOICE_INPUT_MODE: &str = "voice";
const VOICE_NOTE: &str = "[Voice input: this message was dictated via speech transcription and may contain recognition errors. Interpret unclear words from context; ask only when the intent is genuinely ambiguous.]";

pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router(api::routes, api::matches)?;
  registry.websocket("/ws/voice/*", relay::handler)?;
  registry.turn_contribution("voice", Arc::new(VoiceTurnContribution))?;
  registry.template_global("voice_backends", config::voice_backends)?;
  registry.template_global("voice_default_backend", config::voice_default_backend)?;
  registry.config_section(config::section())?;
  registry.config_check("check_default_backend", config::check_default_backend)
}

struct VoiceTurnContribution;

impl TurnContribution for VoiceTurnContribution {
  fn class_name(&self) -> &'static str {
    "VoiceTurnContribution"
  }

  fn input_preamble(&self, _meta: &SessionMetadata, input_event: &Value) -> Option<String> {
    (input_event.get("input_mode").and_then(Value::as_str) == Some(VOICE_INPUT_MODE)).then(|| VOICE_NOTE.to_string())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_server_startup_builds_no_speech_model_and_imports_no_local_engine() {
    let mut registry = Registry::new();

    registry.register_package("src.features.voice", register).expect("register voice");

    for phase in [charliebot_registry::wiring::Phase::Early, charliebot_registry::wiring::Phase::Ready] {
      assert!(registry.services_in(phase).iter().all(|service| service.name() != "speech"));
    }
  }

  #[test]
  fn voice_input_gets_the_python_preamble_byte_for_byte() {
    let contribution = VoiceTurnContribution;
    let meta =
      serde_json::from_value::<SessionMetadata>(serde_json::json!({ "name": "synthetic", "profile": "manager" }))
        .expect("session metadata defaults parse");
    assert_eq!(
      contribution.input_preamble(&meta, &serde_json::json!({ "input_mode": "voice" })),
      Some(VOICE_NOTE.to_string())
    );
    assert_eq!(contribution.input_preamble(&meta, &serde_json::json!({})), None);
  }
}
