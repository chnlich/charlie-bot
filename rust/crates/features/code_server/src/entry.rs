//! The code-server package's registrations: the router, the template global and the config section.

use charliebot_registry::{Registry, RegistryError};

use crate::{api, config};

/// `register`: the router, the template global, then the config section.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router(api::routes, api::matches)?;
  registry.template_global("code_server_enabled", api::code_server_enabled)?;
  registry.config_section(config::section())
}
