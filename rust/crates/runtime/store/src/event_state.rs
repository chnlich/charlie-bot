//! Cache state shared by the chat event reader and Store.

use std::path::PathBuf;
use std::sync::Arc;

use charliebot_core::memo::BoundedMemo;

use crate::event_types::{ChatEvent, SharedChatEvent};
use crate::read_memos::StatSignatureMemo;

pub(crate) const EVENT_COUNT_MEMO_LIMIT: usize = 64;
pub(crate) const EVENT_TAIL_MEMO_LIMIT: usize = 64;
pub(crate) const ARCHIVE_MEMO_LIMIT: usize = 16;
pub(crate) const LIVE_RANGE_MEMO_LIMIT: usize = 4;
pub(crate) const TAIL_WINDOW_SIZE: usize = 512 * 1024;
pub(crate) const LIVE_RANGE_WALK_BYTE_BUDGET: usize = 32 * 1024 * 1024;

/// How many physical lines sit between two byte checkpoints of a log's line index. A checkpoint is a line number and
/// a byte offset, so the index holds about one pair per stride and a seek lands less than a stride before the page: the
/// count scan and [`LineIndex::record_append`] both open a checkpoint within a stride of the last one. On the mock
/// home's 144 MB log that is about 1,020 checkpoints (16 KB of pairs), and a read of at most a stride of lines before
/// the page it serves.
pub(crate) const LINE_INDEX_STRIDE: usize = 64;

/// A log's physical-line index: its line count and the byte where every [`LINE_INDEX_STRIDE`]-th line starts.
///
/// A page far inside a long log seeks to the nearest checkpoint at or before its first line and reads from there, so
/// the read costs the lines from that checkpoint instead of the log's head. A line's number here is the one the `\n`
/// split numbers — a blank line keeps its slot and a final line without a terminator is still a line — so the reader
/// can slice the page out of the lines it read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LineIndex {
  pub(crate) count: usize,
  /// `(line, byte)` pairs in ascending order. The first is `(0, 0)`, and `byte` starts a line.
  pub(crate) checkpoints: Vec<(usize, u64)>,
}

impl LineIndex {
  /// The checkpoint at or before `line`, which is where a read of that line starts.
  pub(crate) fn checkpoint_at(&self, line: usize) -> (usize, u64) {
    let index = self.checkpoints.partition_point(|(checkpoint, _)| *checkpoint <= line);
    self.checkpoints[index.saturating_sub(1)]
  }

  /// Count the line that `byte` starts, opening its checkpoint when its own number is a multiple of the stride or when
  /// the last checkpoint is already a stride behind it.
  ///
  /// The count scan splits a large log into ranges, and each range numbers its checkpoints from its own first line, so
  /// the last checkpoint can sit anywhere within a stride of a multiple. A multiple alone would then leave the next
  /// page almost two strides from its checkpoint. The gap rule keeps every page within one.
  pub(crate) fn record_append(&mut self, byte: u64) {
    let line = self.count;
    let last_checkpoint_line = self.checkpoints.last().map_or(0, |(checkpoint, _)| *checkpoint);
    if line.is_multiple_of(LINE_INDEX_STRIDE) || line.saturating_sub(last_checkpoint_line) >= LINE_INDEX_STRIDE {
      self.checkpoints.push((line, byte));
    }
    self.count += 1;
  }
}
// The head a type walk reads before it parses a line: a type string within it is proven without the rest of the line.
pub(crate) const TYPE_HEAD_PROOF_BYTES: usize = 256;

#[derive(Clone)]
pub(crate) struct LiveRangeMemo {
  pub(crate) mtime_ns: i128,
  pub(crate) size: u64,
  pub(crate) inode: u64,
  // The bounded memo returns the cached list by reference. Arc keeps a live-range hit just as cheap.
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

/// Bounded file-signature memos for the event readers.
pub(crate) struct EventReadCaches {
  pub(crate) count: StatSignatureMemo<PathBuf, LineIndex>,
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
