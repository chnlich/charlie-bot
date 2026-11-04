//! The chat_threads package's sidebar contribution: a chat thread session roots the Threads view.

use charliebot_registry::sidebar::SidebarContribution;
use charliebot_registry::thread_origin::is_thread_session;
use charliebot_store::metadata::SessionMetadata;

/// The sidebar view that a thread session roots. Its task descendants join the view through their parent chain.
pub const THREADS_VIEW: &str = "threads";

pub struct ChatThreadsSidebar;

impl SidebarContribution for ChatThreadsSidebar {
  fn view_member(&self, meta: &SessionMetadata) -> Option<&'static str> {
    is_thread_session(meta).then_some(THREADS_VIEW)
  }
}
