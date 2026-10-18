//! Live voice previews: the Gemini and Muse streams, the browser relay, and its recording archive.

mod archive;
mod audio;
mod config;
mod relay;
mod transcription;

use std::sync::Arc;

use charliebot_registry::turn::TurnContribution;
use charliebot_registry::{Registry, RegistryError};
use charliebot_store::metadata::SessionMetadata;
use serde_json::Value;

const VOICE_INPUT_MODE: &str = "voice";
const VOICE_NOTE: &str = "[Voice input: this message was dictated via speech transcription and may contain recognition errors. Interpret unclear words from context; ask only when the intent is genuinely ambiguous.]";

pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.websocket("/ws/voice/*", relay::handler)?;
  registry.turn_contribution("voice", Arc::new(VoiceTurnContribution))?;
  registry.template_global("voice_backends", config::voice_backends)?;
  registry.template_global("voice_default_backend", config::voice_default_backend)?;
  registry.config_section(config::section())?;
  registry.config_check("check_default_backend", config::check_default_backend)
}

struct VoiceTurnContribution;

impl TurnContribution for VoiceTurnContribution {
  fn input_preamble(&self, _meta: &SessionMetadata, input_event: &Value) -> Option<String> {
    (input_event.get("input_mode").and_then(Value::as_str) == Some(VOICE_INPUT_MODE)).then(|| VOICE_NOTE.to_string())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

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
