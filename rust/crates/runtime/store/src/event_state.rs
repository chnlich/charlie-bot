//! Cache state shared by the chat event reader and Store.

use std::path::PathBuf;
use std::sync::Arc;

use charliebot_core::memo::BoundedMemo;

use crate::event_types::{ChatEvent, SharedChatEvent};
use crate::read_memos::StatSignatureMemo;

// Match src/infra/ndjson.py:_COUNT_MEMO_LIMIT and its (mtime_ns, size) signatures.
pub(crate) const EVENT_COUNT_MEMO_LIMIT: usize = 64;
pub(crate) const EVENT_TAIL_MEMO_LIMIT: usize = 64;
// Match src/runtime/chat_events.py:_ARCHIVE_MEMO_LIMIT for both archive memos.
pub(crate) const ARCHIVE_MEMO_LIMIT: usize = 16;
// Match src/runtime/chat_events.py:_LIVE_RANGE_MEMO_LIMIT and _WALK_BYTE_BUDGET.
pub(crate) const LIVE_RANGE_MEMO_LIMIT: usize = 4;
pub(crate) const TAIL_WINDOW_SIZE: usize = 512 * 1024;
pub(crate) const LIVE_RANGE_WALK_BYTE_BUDGET: usize = 32 * 1024 * 1024;
// The head a type walk reads before it parses a line: a type string within it is proven without the rest of the line.
pub(crate) const TYPE_HEAD_PROOF_BYTES: usize = 256;

#[derive(Clone)]
pub(crate) struct LiveRangeMemo {
  pub(crate) mtime_ns: i128,
  pub(crate) size: u64,
  pub(crate) inode: u64,
  // Python's BoundedMemo returns the cached list by reference. Arc keeps a live-range hit just as cheap.
  pub(crate) lines: Arc<Vec<Option<Arc<ChatEvent>>>>,
  pub(crate) covered_byte_end: usize,
  pub(crate) ends_on_boundary: bool,
  pub(crate) first_line_index: usize,
  pub(crate) covered_byte_start: usize,
}

pub(crate) type ParsedLiveLine = Option<Arc<ChatEvent>>;
pub(crate) type SharedLiveLines = Arc<Vec<ParsedLiveLine>>;
pub(crate) type TailPage = (Arc<Vec<ChatEvent>>, usize, bool);

pub(crate) struct TailLineWalk {
  pub(crate) lines: Vec<Vec<u8>>,
  pub(crate) byte_start: usize,
  pub(crate) ends_on_boundary: bool,
  pub(crate) bytes_read: usize,
}

/// Bounded file-signature memos for the Python-equivalent event readers.
pub(crate) struct EventReadCaches {
  pub(crate) count: StatSignatureMemo<PathBuf, usize>,
  pub(crate) tail: StatSignatureMemo<(PathBuf, usize), TailPage>,
  pub(crate) archive_paths: StatSignatureMemo<PathBuf, Arc<Vec<PathBuf>>>,
  pub(crate) archive_events: StatSignatureMemo<PathBuf, Arc<Vec<SharedChatEvent>>>,
  pub(crate) live_ranges: BoundedMemo<PathBuf, LiveRangeMemo>,
  #[cfg(test)]
  pub(crate) tail_bytes_read: usize,
  #[cfg(test)]
  pub(crate) tail_lines_parsed: usize,
  #[cfg(test)]
  pub(crate) live_range_bytes_read: usize,
  #[cfg(test)]
  pub(crate) live_range_lines_parsed: usize,
  #[cfg(test)]
  pub(crate) archive_file_reads: usize,
  #[cfg(test)]
  pub(crate) archive_dir_walks: usize,
  #[cfg(test)]
  pub(crate) count_bytes_read: usize,
}

impl Default for EventReadCaches {
  fn default() -> Self {
    Self {
      count: StatSignatureMemo::new(EVENT_COUNT_MEMO_LIMIT),
      tail: StatSignatureMemo::new(EVENT_TAIL_MEMO_LIMIT),
      archive_paths: StatSignatureMemo::new(ARCHIVE_MEMO_LIMIT),
      archive_events: StatSignatureMemo::new(ARCHIVE_MEMO_LIMIT),
      live_ranges: BoundedMemo::new(LIVE_RANGE_MEMO_LIMIT),
      #[cfg(test)]
      tail_bytes_read: 0,
      #[cfg(test)]
      tail_lines_parsed: 0,
      #[cfg(test)]
      live_range_bytes_read: 0,
      #[cfg(test)]
      live_range_lines_parsed: 0,
      #[cfg(test)]
      archive_file_reads: 0,
      #[cfg(test)]
      archive_dir_walks: 0,
      #[cfg(test)]
      count_bytes_read: 0,
    }
  }
}
