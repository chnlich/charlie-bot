//! `src/features/code_server`: the `code_server:` config section, the `code_server_enabled` template global that shows
//! the diff page's open button, and the `/api/code-server/open` route that opens a folder in code-server.

// The helpers answer a failure as the HTTP response it sends, as `charliebot_registry::caller` does, so their
// `Err` is a `Response`.
#![allow(clippy::result_large_err)]

mod api;
mod config;
mod which;

use charliebot_registry::{Registry, RegistryError};

/// `register` (`src/features/code_server/__init__.py`): the router, the template global, then the config section.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router(api::routes, api::matches)?;
  registry.template_global("code_server_enabled", api::code_server_enabled)?;
  registry.config_section(config::section())
}

#[cfg(test)]
mod tests;
