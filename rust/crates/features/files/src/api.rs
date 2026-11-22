//! The file server's public surface: the registration the package list calls.

use charliebot_registry::pages::HomeCard;
use charliebot_registry::{Registry, RegistryError};

use crate::internals::{matches, routes};

/// `register`: the router, then the home card.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router(routes, matches)?;
  registry.home_card(HomeCard {
    name: "File browser",
    url: "/absolute_filepath/",
    description: "Browse any file on this host's filesystem.",
  })
}
