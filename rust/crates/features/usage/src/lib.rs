//! `src/features/usage`: its page and API ports are pending; its home card remains registered.

use charliebot_registry::pages::HomeCard;
use charliebot_registry::{Registry, RegistryError};

pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.home_card(HomeCard {
    name: "Token usage by model",
    url: "/token-usage",
    description: "Tokens per model across every agent log on this host.",
  })
}
