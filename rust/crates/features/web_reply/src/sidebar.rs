//! The web reply package's sidebar contribution: the `has_waiting_items` flag of each row, which says the session
//! stores at least one Waiting on you item.

use std::path::Path;

use charliebot_core::metadata_slot::METADATA_FILE_NAME;
use charliebot_registry::sidebar::SidebarContribution;
use charliebot_store::layout;
use charliebot_store::metadata::read_session_metadata;
use charliebot_store::sidebar_state::HAS_WAITING_ITEMS;

use crate::status::has_stored_waiting;

/// The web reply package's sidebar contribution. Its one flag reads the session's stored status.
pub(crate) struct WebReplySidebar;

impl SidebarContribution for WebReplySidebar {
  fn watched_files(&self) -> &'static [&'static str] {
    &[METADATA_FILE_NAME]
  }

  fn row_flags(&self, session_dir: &Path, session_id: &str) -> Vec<(&'static str, bool)> {
    match stored_waiting(session_dir) {
      Ok(waiting) => vec![(HAS_WAITING_ITEMS, waiting)],
      // A row contribution cannot fail, so the escape is logged and the row gets no flag.
      Err(error) => {
        tracing::error!(session_id, error = %error, "web_status_read_failed");
        Vec::new()
      }
    }
  }
}

/// Whether the session's metadata file stores a Waiting on you item. A session without a metadata file stores none.
fn stored_waiting(session_dir: &Path) -> anyhow::Result<bool> {
  let path = layout::metadata_path(session_dir);
  let meta =
    read_session_metadata(&path).map_err(|error| anyhow::anyhow!("{} is unreadable: {error:?}", path.display()))?;
  match meta {
    Some(meta) => has_stored_waiting(&meta),
    None => Ok(false),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_corrupt_metadata_file_yields_no_flag_and_a_missing_one_yields_false() {
    let dir = tempfile::tempdir().expect("temporary directory exists");
    assert_eq!(WebReplySidebar.row_flags(dir.path(), "missing"), vec![(HAS_WAITING_ITEMS, false)]);
    std::fs::write(layout::metadata_path(dir.path()), "{not json").expect("the corrupt metadata writes");
    assert!(WebReplySidebar.row_flags(dir.path(), "corrupt").is_empty());
  }
}
