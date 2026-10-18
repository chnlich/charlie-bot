//! `src/features/diff_view`: the page route is pending; its home card remains registered.

use charliebot_registry::pages::HomeCard;
use charliebot_registry::{Registry, RegistryError};

pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.home_card(HomeCard {
    name: "Diff viewer",
    url: "/diff",
    description: "Browse a repository diff between two refs.",
  })
}
