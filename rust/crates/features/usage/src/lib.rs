//! `src/features/usage`: the quota panel (`ext_usage`) is ported. The usage page, the usage tally and the cold-storage
//! sweep are pending, and the home card stays registered.

mod api;
mod ext_usage;
mod poller;

use std::sync::Arc;

use charliebot_registry::pages::HomeCard;
use charliebot_registry::{Registry, RegistryError};

/// `register` (`src/features/usage/__init__.py`): the `/api` router of `ext_usage`, the `ext_usage` service in phase
/// Ready, then the home card.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router(api::routes, api::matches)?;
  registry.service(Arc::new(poller::ExtUsagePoller::default()))?;
  registry.home_card(HomeCard {
    name: "Token usage by model",
    url: "/token-usage",
    description: "Tokens per model across every agent log on this host.",
  })
}

#[cfg(test)]
mod tests {
  use charliebot_registry::wiring::Phase;

  use super::*;

  #[test]
  fn register_mounts_the_router_starts_the_service_and_lists_the_card() {
    let mut registry = Registry::new();
    registry.register_package("src.features.usage", register).expect("the package registers");
    assert_eq!(registry.routers().len(), 1);
    let services = registry.services_in(Phase::Ready);
    assert_eq!(services.iter().map(|service| service.name()).collect::<Vec<_>>(), ["ext_usage"]);
    assert!(registry.services_in(Phase::Early).is_empty());
    let cards = registry.home_cards();
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0].url, "/token-usage");
  }
}
