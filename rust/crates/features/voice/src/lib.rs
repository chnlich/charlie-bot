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
mod recordings;
mod relay;
mod transcription;
mod wav;

pub use file::transcribe_file;

use std::sync::Arc;

use charliebot_core::event_types;
use charliebot_registry::pages::HomeCard;
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
  registry.template_global_stable("voice_backends", config::voice_backends)?;
  registry.template_global_stable("voice_default_backend", config::voice_default_backend)?;
  registry.config_section(config::section())?;
  registry.config_check("check_default_backend", config::check_default_backend)?;
  registry.home_card(HomeCard {
    name: "Recent recordings",
    url: api::RECORDINGS_PAGE_PATH,
    description: "Recent voice recordings across every session, playable, with each run's startup report and failure.",
  })
}

struct VoiceTurnContribution;

impl TurnContribution for VoiceTurnContribution {
  fn class_name(&self) -> &'static str {
    "VoiceTurnContribution"
  }

  fn input_preamble(&self, _meta: &SessionMetadata, input_event: &Value) -> Option<String> {
    (input_event.get(event_types::INPUT_MODE).and_then(Value::as_str) == Some(VOICE_INPUT_MODE))
      .then(|| VOICE_NOTE.to_string())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_server_startup_builds_no_speech_model_and_imports_no_local_engine() {
    let mut registry = Registry::new();

    registry.register_package("features.voice", register).expect("register voice");

    for phase in [charliebot_registry::wiring::Phase::Early, charliebot_registry::wiring::Phase::Ready] {
      assert!(registry.services_in(phase).iter().all(|service| service.name() != "speech"));
    }
  }

  #[test]
  fn register_lists_the_recent_recordings_home_card() {
    let mut registry = Registry::new();

    registry.register_package("features.voice", register).expect("register voice");

    let cards = registry.home_cards();
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0].name, "Recent recordings");
    assert_eq!(cards[0].url, "/voice-recordings");
  }

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
