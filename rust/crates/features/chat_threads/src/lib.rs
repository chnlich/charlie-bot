//! `src/features/chat_threads`: the Threads sidebar subtree contribution.

mod sidebar;
mod thread_sessions;

use std::sync::Arc;

use charliebot_registry::{Registry, RegistryError};

pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.sidebar_contribution("chat_threads", Arc::new(sidebar::ChatThreadsSidebar))
}
