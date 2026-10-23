//! `src/features/chat_threads`: the Slack and Discord thread sessions. Their turn contribution names the thread brief,
//! their sidebar subtree is the Threads view, and `GET /api/sessions/chat-threads` lists that view. The reply contract
//! that the platform turn contributions share is in `thread_entry`.

mod api;
pub mod backfill;
pub mod channel_service;
mod sidebar;
#[cfg(feature = "test-support")]
pub mod test_support;
pub mod thread_entry;
mod thread_sessions;
mod turn_contribution;

use std::sync::Arc;

use charliebot_registry::{Registry, RegistryError};

/// Register in the order of `src/features/chat_threads/__init__.py`: the turn contribution, the router, then the
/// sidebar contribution.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.turn_contribution("chat_threads", Arc::new(turn_contribution::ChatThreadsTurn))?;
  registry.router(api::routes, api::matches)?;
  registry.sidebar_contribution("chat_threads", Arc::new(sidebar::ChatThreadsSidebar))
}

#[cfg(test)]
mod tests {
  use charliebot_registry::Registry;
  use charliebot_store::metadata::{SessionMetadata, TaskProfile};
  use serde_json::json;

  #[test]
  fn register_adds_the_turn_contribution_the_router_and_the_sidebar_view() {
    let mut registry = Registry::new();
    registry.register_package("src.features.chat_threads", super::register).expect("the package registers");
    assert_eq!(registry.routers().len(), 1);
    assert_eq!(registry.sidebar_contributions().count(), 1);

    let mut thread = SessionMetadata::new("synthetic", "Synthetic", TaskProfile::Manager);
    thread.set_extra("slack_origin", json!({"channel_id": "C0SYNTHETIC"}));
    assert_eq!(registry.workflow_rules_file(&thread).expect("one contribution names a file"), "thread_session.md");
  }
}
