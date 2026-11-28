//! The chat_threads package's sidebar contribution: a top-level chat thread session roots the Threads overlay.

use charliebot_registry::sidebar::SidebarContribution;
use charliebot_registry::thread_origin::is_thread_session;
use charliebot_store::metadata::SessionMetadata;

/// The sidebar view that a top-level thread session roots. A thread session with a task parent is not a root: it
/// joins the view only through a parent chain that reaches a top-level thread session, so one under a session outside
/// the view lists in the Workspace. Task descendants join the view through their parent chain.
pub const THREADS_VIEW: &str = "threads";

pub struct ChatThreadsSidebar;

impl SidebarContribution for ChatThreadsSidebar {
  fn view_member(&self, meta: &SessionMetadata) -> Option<&'static str> {
    (is_thread_session(meta) && meta.task_parent_id.is_none()).then_some(THREADS_VIEW)
  }
}
