//! `src/features/chat_threads`: the Threads sidebar subtree contribution.

use std::sync::Arc;

use charliebot_registry::{Registry, RegistryError, sidebar::SidebarContribution};
use charliebot_store::metadata::SessionMetadata;

const THREADS_VIEW: &str = "threads";

struct ChatThreadsSidebar;

impl SidebarContribution for ChatThreadsSidebar {
  fn view_member(&self, meta: &SessionMetadata) -> Option<&'static str> {
    let has_slack_origin = meta.extra("slack_origin").is_some_and(|value| !value.is_null());
    let has_discord_origin = meta.extra("discord_origin").is_some_and(|value| !value.is_null());
    (has_slack_origin || has_discord_origin).then_some(THREADS_VIEW)
  }
}

pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.sidebar_contribution("chat_threads", Arc::new(ChatThreadsSidebar))
}
