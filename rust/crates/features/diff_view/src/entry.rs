//! The diff-view package's registrations: the `/api/git` router, the page router and the home card.

use charliebot_registry::pages::HomeCard;
use charliebot_registry::{Registry, RegistryError};

use crate::{api, page};

/// `register`: the `/api/git` router, the page router, then the home card.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router(api::routes, api::matches)?;
  registry.router(page::routes, page::matches)?;
  registry.home_card(HomeCard {
    name: "Diff viewer",
    url: page::PAGE_PATH,
    description: "Browse a repository diff between two refs.",
  })
}
