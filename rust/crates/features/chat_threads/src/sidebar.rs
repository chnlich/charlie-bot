//! The chat_threads package's sidebar contribution (`src/features/chat_threads/sidebar.py`): a Slack or Discord thread
//! session roots the Threads view.

use charliebot_registry::sidebar::SidebarContribution;
use charliebot_store::metadata::SessionMetadata;

use crate::thread_sessions;

/// The sidebar view that a thread session roots. Its task descendants join the view through their parent chain.
pub const THREADS_VIEW: &str = "threads";

pub struct ChatThreadsSidebar;

impl SidebarContribution for ChatThreadsSidebar {
  fn view_member(&self, meta: &SessionMetadata) -> Option<&'static str> {
    thread_sessions::is_thread_session(meta).then_some(THREADS_VIEW)
  }
}
