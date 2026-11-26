//! The chat-threads package's registrations: the router and the sidebar view.

use std::sync::Arc;

use charliebot_registry::{Registry, RegistryError};

use crate::{api, sidebar};

/// `register`: the router, then the sidebar contribution.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router(api::routes, api::matches)?;
  registry.sidebar_contribution("chat_threads", Arc::new(sidebar::ChatThreadsSidebar))
}
