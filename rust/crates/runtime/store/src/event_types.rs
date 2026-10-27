//! Chat event values and the live event-log identity shared by store readers.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use crate::compact::CompactValue;
use crate::{StoreError, StoreResult};

/// One parsed chat-log event: the compact interned-key form of its JSON.
pub type ChatEvent = CompactValue;

/// A reference counted parsed event shared by the live cache and task facts.
pub type SharedChatEvent = Arc<ChatEvent>;

/// The identity of a live event log on disk: its inode, size and modification time, as Python's
/// `_task_state_fast` keys its memo. An append moves the size, and a rewrite publishes a new inode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FileIdentity {
  pub(crate) inode: u64,
  pub(crate) size: u64,
  pub(crate) mtime_ns: i128,
}

impl FileIdentity {
  /// The byte length the identity covers: the offset a walk of this file starts from.
  pub(crate) fn size(&self) -> u64 {
    self.size
  }

  /// The identity of the file at `path`, or None when no file is there.
  pub(crate) fn read(path: &Path) -> StoreResult<Option<Self>> {
    use std::os::unix::fs::MetadataExt;
    match fs::metadata(path) {
      Ok(metadata) => Ok(Some(Self {
        inode: metadata.ino(),
        size: metadata.size(),
        mtime_ns: i128::from(metadata.mtime()) * 1_000_000_000 + i128::from(metadata.mtime_nsec()),
      })),
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
      Err(error) => Err(StoreError::new(format!("{}: {error}", path.display()))),
    }
  }
}
