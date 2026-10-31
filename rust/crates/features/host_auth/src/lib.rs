//! `src/features/host_auth`: the host login panel. A standing poller probes every ssh alias in the operator's config,
//! keeps the result in `<home>/host_auth.json`, and serves it at `/host-auth` and `/api/host-auth/*`.

mod api;
mod host_auth;
mod page;
mod poller;
mod probe;
mod ssh_config;
mod timestamps;

use std::sync::Arc;

use charliebot_registry::pages::HomeCard;
use charliebot_registry::{Registry, RegistryError};

/// `register` (`src/features/host_auth/__init__.py`): the router, the `host_auth` service in phase Ready, then the
/// home card.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router(api::routes, api::matches)?;
  registry.service(Arc::new(poller::HostAuthPoller::default()))?;
  registry.home_card(HomeCard {
    name: "Host login authorization",
    url: "/host-auth",
    description: "Per-host ssh login state and the estimated Okta renewal deadline.",
  })
}

#[cfg(test)]
mod tests {
  use charliebot_registry::wiring::Phase;

  use super::*;

  #[test]
  fn register_mounts_the_router_starts_the_service_and_lists_the_card() {
    let mut registry = Registry::new();
    registry.register_package("src.features.host_auth", register).expect("the package registers");
    assert_eq!(registry.routers().len(), 1);
    let services = registry.services_in(Phase::Ready);
    assert_eq!(services.iter().map(|service| service.name()).collect::<Vec<_>>(), ["host_auth"]);
    assert!(registry.services_in(Phase::Early).is_empty());
    let cards = registry.home_cards();
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0].url, "/host-auth");
    assert_eq!(cards[0].name, "Host login authorization");
  }
}
