//! The approval day view feature (`web/features/approval`): the page at `GET /approval` and the route the page
//! reads, `GET /api/approval/day`. Both cover one Pacific day's ranked T0 queue, login sheet and digest.

mod api;
mod day;
mod page;
#[cfg(test)]
mod test_support;

use charliebot_registry::pages::HomeCard;
use charliebot_registry::{Registry, RegistryError};

/// The feature's registration: the day API's router, the page's router, then the home card.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router(api::routes, api::matches)?;
  registry.router(page::routes, page::matches)?;
  registry.home_card(HomeCard {
    name: "Approvals",
    url: page::PAGE_PATH,
    description: "One Pacific day of the tiered approval: the Waiting on you queue, the login sheet and the digest.",
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn register_mounts_the_routers_and_lists_the_home_card() {
    let mut registry = Registry::new();
    registry.register_package("features.approval", register).expect("the package registers");
    assert_eq!(registry.routers().len(), 2);
    let cards = registry.home_cards();
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0].name, "Approvals");
    assert_eq!(cards[0].url, "/approval");
  }
}
