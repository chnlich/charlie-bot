//! The file server's public surface: the mount prefix, and the registration the package list calls.

use charliebot_registry::pages::HomeCard;
use charliebot_registry::{Registry, RegistryError};

use crate::internals::{matches, routes};

/// `file_urls.FILE_SERVER_MOUNTS[0]`: the prefix the route is mounted under, without its trailing slash.
pub const MOUNT: &str = charliebot_compat::file_urls::FILE_SERVER_MOUNT;

/// `register`: the router, then the home card.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router(routes, matches)?;
  registry.home_card(HomeCard {
    name: "File browser",
    url: "/absolute_filepath/",
    description: "Browse any file on this host's filesystem.",
  })
}
