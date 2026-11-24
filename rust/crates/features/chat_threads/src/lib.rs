//! `web/features/chat_threads`: the chat thread sessions. The subtree of each top-level thread session is the Threads
//! view, and `GET /api/sessions/chat-threads` lists that view. The reply contract that the platform turn contributions
//! share is in `thread_entry`, the window their read commands
//! answer with is in `thread_read`, and the chat line their reply events render is in `reply_event`.

mod api;
pub mod backfill;
pub mod channel_service;
pub mod listener_connection;
pub mod reply_event;
mod sidebar;
pub mod summon;
#[cfg(feature = "test-support")]
pub mod test_support;
pub mod thread_entry;
pub mod thread_read;

use std::sync::Arc;

use charliebot_registry::{Registry, RegistryError};

/// `register`: the router, then the sidebar contribution.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router(api::routes, api::matches)?;
  registry.sidebar_contribution("chat_threads", Arc::new(sidebar::ChatThreadsSidebar))
}

#[cfg(test)]
mod tests {
  use charliebot_registry::Registry;

  #[test]
  fn register_adds_the_router_and_the_sidebar_view() {
    let mut registry = Registry::new();
    registry.register_package("features.chat_threads", super::register).expect("the package registers");
    assert_eq!(registry.routers().len(), 1);
    assert_eq!(registry.sidebar_contributions().count(), 1);
  }
}
