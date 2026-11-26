//! The web reply package's sidebar contribution: the `has_waiting_items` flag of each row, which says the session
//! stores a Waiting on you item that no recorded click answered.

use charliebot_core::metadata_slot::METADATA_FILE_NAME;
use charliebot_registry::sidebar::SidebarContribution;
use charliebot_store::Store;
use charliebot_store::layout;
use charliebot_store::metadata::read_session_metadata;
use charliebot_store::sidebar_state::HAS_WAITING_ITEMS;

use crate::status::has_unanswered_waiting;

/// The web reply package's sidebar contribution. Its one flag reads the session's stored status and its click facts.
pub(crate) struct WebReplySidebar;

impl SidebarContribution for WebReplySidebar {
  fn watched_files(&self) -> &'static [&'static str] {
    &[METADATA_FILE_NAME]
  }

  fn row_flags(&self, store: &Store, session_id: &str) -> Vec<(&'static str, bool)> {
    match stored_unanswered(store, session_id) {
      Ok(waiting) => vec![(HAS_WAITING_ITEMS, waiting)],
      // A row contribution cannot fail, so the escape is logged and the row gets no flag.
      Err(error) => {
        tracing::error!(session_id, error = %error, "web_status_read_failed");
        Vec::new()
      }
    }
  }
}

/// Whether the session's metadata file stores a Waiting on you item that no click answered. A session without a
/// metadata file stores none.
fn stored_unanswered(store: &Store, session_id: &str) -> anyhow::Result<bool> {
  let path = layout::metadata_path(&store.session_dir(session_id));
  let meta =
    read_session_metadata(&path).map_err(|error| anyhow::anyhow!("{} is unreadable: {error:?}", path.display()))?;
  match meta {
    Some(meta) => has_unanswered_waiting(store, &meta),
    None => Ok(false),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::test_support::Home;

  #[test]
  fn a_corrupt_metadata_file_yields_no_flag_and_a_missing_one_yields_false() {
    let home = Home::new();
    let store = &home.features.store;
    assert_eq!(WebReplySidebar.row_flags(store, "missing"), vec![(HAS_WAITING_ITEMS, false)]);
    let dir = store.session_dir("corrupt");
    std::fs::create_dir_all(&dir).expect("the session directory exists");
    std::fs::write(layout::metadata_path(&dir), "{not json").expect("the corrupt metadata writes");
    assert!(WebReplySidebar.row_flags(store, "corrupt").is_empty());
  }
}
