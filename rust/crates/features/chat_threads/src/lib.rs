//! `src/features/chat_threads`: the Threads sidebar subtree and the thread brief of a thread session.

mod sidebar;
mod thread_sessions;
mod turn_contribution;

use std::sync::Arc;

use charliebot_registry::{Registry, RegistryError};

/// Register in the order of `src/features/chat_threads/__init__.py`: the turn contribution, then the sidebar
/// contribution.
pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.turn_contribution("chat_threads", Arc::new(turn_contribution::ChatThreadsTurn))?;
  registry.sidebar_contribution("chat_threads", Arc::new(sidebar::ChatThreadsSidebar))
}

#[cfg(test)]
mod tests {
  use charliebot_registry::Registry;
  use charliebot_store::metadata::{SessionMetadata, TaskProfile};
  use serde_json::json;

  #[test]
  fn register_names_the_thread_brief_and_roots_the_threads_view() {
    let mut registry = Registry::new();
    registry.register_package("src.features.chat_threads", super::register).expect("the package registers");
    assert_eq!(registry.sidebar_contributions().count(), 1);

    let mut thread = SessionMetadata::new("synthetic", "Synthetic", TaskProfile::Manager);
    thread.set_extra("slack_origin", json!({"channel_id": "C0SYNTHETIC"}));
    assert_eq!(registry.workflow_rules_file(&thread).expect("one contribution names a file"), "thread_session.md");
  }
}
