//! The voice turn contribution: the note that the model reads before a turn whose input was dictated.

use charliebot_core::event_types;
use charliebot_registry::turn::TurnContribution;
use charliebot_store::metadata::SessionMetadata;
use serde_json::Value;

const VOICE_INPUT_MODE: &str = "voice";
const VOICE_NOTE: &str = "[Voice input: this message was dictated via speech transcription and may contain recognition errors. Interpret unclear words from context; ask only when the intent is genuinely ambiguous.]";

pub(crate) struct VoiceTurnContribution;

impl TurnContribution for VoiceTurnContribution {
  fn input_preamble(&self, _meta: &SessionMetadata, input_event: &Value) -> Option<String> {
    (input_event.get(event_types::INPUT_MODE).and_then(Value::as_str) == Some(VOICE_INPUT_MODE))
      .then(|| VOICE_NOTE.to_string())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn voice_input_gets_the_preamble_byte_for_byte() {
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
