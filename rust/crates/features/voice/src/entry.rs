//! The registration entry: the one item the package registry calls to mount the voice feature.

use std::sync::Arc;

use charliebot_registry::pages::HomeCard;
use charliebot_registry::{Registry, RegistryError};

use crate::turn::VoiceTurnContribution;
use crate::{api, config, relay};

pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router(api::routes, api::matches)?;
  registry.websocket("/ws/voice/*", relay::handler)?;
  registry.turn_contribution("voice", Arc::new(VoiceTurnContribution))?;
  registry.template_global_stable("voice_backends", config::voice_backends)?;
  registry.template_global_stable("voice_default_backend", config::voice_default_backend)?;
  registry.config_section(config::section())?;
  registry.home_card(HomeCard {
    name: "Recent recordings",
    url: api::RECORDINGS_PAGE_PATH,
    description: "Recent voice recordings across every session, playable, with each run's startup report and failure.",
  })
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
}
