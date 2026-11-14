//! Chat event values and the live event-log identity shared by store readers.

use std::ffi::{CStr, CString};
use std::fs;
use std::os::fd::AsFd;
use std::path::Path;
use std::sync::Arc;

use charliebot_core::timefmt;
use nix::errno::Errno;
use nix::fcntl::AtFlags;
use nix::sys::stat::fstatat;

use crate::compact::CompactValue;
use crate::{StoreError, StoreResult};

/// One parsed chat-log event: the compact interned-key form of its JSON.
pub type ChatEvent = CompactValue;

/// A reference counted parsed event shared by the live cache and task facts.
pub type SharedChatEvent = Arc<ChatEvent>;

/// The identity of a live event log on disk: its inode, size and modification time.
/// An append moves the size, and a rewrite publishes a new inode.
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
        mtime_ns: timefmt::file_time_ns(metadata.mtime(), metadata.mtime_nsec()),
      })),
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
      Err(error) => Err(StoreError::new(format!("{}: {error}", path.display()))),
    }
  }

  /// The identity of the file at `relative`, resolved below the open directory `dir`.
  ///
  /// The read is the same `fstatat` the path form makes. Anchoring it on a directory handle means a
  /// page's hundred identity reads walk only the session's own components, not the home's whole path.
  pub(crate) fn read_at(dir: &impl AsFd, relative: &str) -> StoreResult<Option<Self>> {
    let path = CString::new(relative).map_err(|_| StoreError::new(format!("{relative}: the path holds a NUL byte")))?;
    Self::read_at_cstr(dir, &path)
  }

  /// The identity of the file at `relative`, for a scan that builds its paths in one reused buffer.
  pub(crate) fn read_at_cstr(dir: &impl AsFd, relative: &CStr) -> StoreResult<Option<Self>> {
    match fstatat(dir, relative, AtFlags::empty()) {
      Ok(stat) => Ok(Some(Self {
        inode: stat.st_ino,
        size: stat.st_size as u64,
        mtime_ns: timefmt::file_time_ns(stat.st_mtime, stat.st_mtime_nsec),
      })),
      Err(Errno::ENOENT) => Ok(None),
      Err(error) => Err(StoreError::new(format!("{}: {error}", relative.to_string_lossy()))),
    }
  }
}
