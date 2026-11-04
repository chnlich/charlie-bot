//! The startup-only fact checkpoint of one session's history.
//!
//! A boot's fact scan folds a session's archived events and then its live log into the node and run facts. The
//! checkpoint holds what the scan kept from that history: one line per kept event, in log order, each in the compact
//! form the scan built (a chat-content line keeps only its id and type). A later boot folds those lines and the bytes
//! appended past the checkpoint's offset, so it reaches the facts a full fold reaches while parsing the checkpoint's
//! fraction of the log.
//!
//! The file is startup-only and is never synced. Every read validates it against its sources before use: the archived
//! extent, each archive file's name, size and modification time, the live log's length (which must cover the offset),
//! the line boundary at the offset, and the SHA-256 of the first and last [`WINDOW_BYTES`] of the covered prefix. A
//! checkpoint that is absent, truncated, torn, stale or written against another log fails that check, and the boot
//! scans the log whole. That full scan is also the recovery a missing checkpoint gives, so a torn or stale copy costs
//! a slower boot and nothing else.
//!
//! The event lines are parsed with the scan's own line rule, so the parse sees each line as the scan saw it: a stand-in
//! keeps its id and type, a kept line parses whole.

use std::path::Path;

use charliebot_core::fsutil;
use charliebot_core::text_repr::{self, JsonStyle};
use serde_json::{Map, Value, json};

use crate::event_types::{FileIdentity, SharedChatEvent};
use crate::{StoreError, StoreResult};

/// The format version of the checkpoint file. A file of another version is not read.
const CHECKPOINT_VERSION: u64 = 1;
/// The bytes of each end of the covered prefix whose digest the checkpoint keeps. A rewrite that leaves both windows
/// and the length intact would have to reproduce the prefix's ends, which no writer here does.
pub(crate) const WINDOW_BYTES: usize = 4096;
/// The checkpoint's JSON style: one event per line, no spaces between members.
const CHECKPOINT_JSON: JsonStyle =
  JsonStyle { ensure_ascii: false, sort_keys: false, item_separator: ",", key_separator: ":", indent: None };

/// One archive file the checkpoint's facts folded, identified by the values a rename or append changes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ArchiveMark {
  pub(crate) name: String,
  pub(crate) size: u64,
  pub(crate) mtime_ns: i128,
}

impl ArchiveMark {
  pub(crate) fn of(path: &Path) -> StoreResult<Option<Self>> {
    let Some(identity) = FileIdentity::read(path)? else { return Ok(None) };
    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default().to_string();
    Ok(Some(Self { name, size: identity.size(), mtime_ns: identity.mtime_ns }))
  }
}

/// The events a scan kept from the prefix of one history, and the prefix they cover.
pub(crate) struct FactCheckpoint {
  /// The archived events among the kept lines. The scan keeps exactly the session's `archive_offset` events from its
  /// archive files, so this is also where the live events' fold begins.
  pub(crate) archived: usize,
  /// The live events among the kept lines.
  pub(crate) covered: usize,
  /// The live-log bytes the prefix covers. Its last byte is a line end.
  pub(crate) offset: u64,
  head_sha: String,
  tail_sha: String,
  pub(crate) archive_marks: Vec<ArchiveMark>,
}

impl FactCheckpoint {
  /// The checkpoint a scan of `file` left: the kept event lines, the prefix they cover and the archive files that
  /// prefix folded.
  pub(crate) fn new(archived: usize, covered: usize, archive_marks: Vec<ArchiveMark>, file: &[u8]) -> Self {
    Self {
      archived,
      covered,
      offset: file.len() as u64,
      head_sha: window_sha(file, 0),
      tail_sha: window_sha(file, file.len()),
      archive_marks,
    }
  }

  /// Whether this checkpoint's events cover a prefix of `live`: the archived extent and archive files are the ones it
  /// folded, the file still holds its offset bytes, a line ends there, and both prefix windows keep their digests.
  pub(crate) fn covers(&self, live: &[u8], archives: usize, archive_marks: &[ArchiveMark]) -> bool {
    let offset = self.offset as usize;
    self.archived == archives
      && self.archive_marks == archive_marks
      && live.len() >= offset
      && (offset == 0 || live[offset - 1] == b'\n')
      && self.head_sha == window_sha(live, 0)
      && self.tail_sha == window_sha(live, offset)
  }

  /// The file's text: a header line, then the event lines.
  pub(crate) fn to_bytes(&self, events_json: &str) -> String {
    let mut header = Map::new();
    header.insert("version".to_string(), json!(CHECKPOINT_VERSION));
    header.insert("archived".to_string(), json!(self.archived));
    header.insert("covered".to_string(), json!(self.covered));
    header.insert("offset".to_string(), json!(self.offset));
    header.insert("head_sha".to_string(), Value::String(self.head_sha.clone()));
    header.insert("tail_sha".to_string(), Value::String(self.tail_sha.clone()));
    header.insert(
      "archives".to_string(),
      Value::Array(
        self
          .archive_marks
          .iter()
          .map(|mark| {
            json!({"name": mark.name, "size": mark.size, "mtime_ns": i64::try_from(mark.mtime_ns).unwrap_or_default()})
          })
          .collect(),
      ),
    );
    let mut text = text_repr::json_dumps(&Value::Object(header), &CHECKPOINT_JSON);
    text.push('\n');
    text.push_str(events_json);
    text
  }

  /// The checkpoint a file's bytes hold, with the byte offset where its event lines begin, or `None` when the bytes
  /// are not a checkpoint of this version.
  pub(crate) fn parse(raw: &[u8]) -> Option<(Self, usize)> {
    let newline = raw.iter().position(|byte| *byte == b'\n')?;
    let value: Value = serde_json::from_slice(&raw[..newline]).ok()?;
    let object = value.as_object()?;
    if object.get("version")?.as_u64()? != CHECKPOINT_VERSION {
      return None;
    }
    let archive_marks = object
      .get("archives")?
      .as_array()?
      .iter()
      .map(|mark| {
        let mark = mark.as_object()?;
        Some(ArchiveMark {
          name: mark.get("name")?.as_str()?.to_string(),
          size: mark.get("size")?.as_u64()?,
          mtime_ns: i128::from(mark.get("mtime_ns")?.as_i64()?),
        })
      })
      .collect::<Option<Vec<_>>>()?;
    let checkpoint = Self {
      archived: usize::try_from(object.get("archived")?.as_u64()?).ok()?,
      covered: usize::try_from(object.get("covered")?.as_u64()?).ok()?,
      offset: object.get("offset")?.as_u64()?,
      head_sha: object.get("head_sha")?.as_str()?.to_string(),
      tail_sha: object.get("tail_sha")?.as_str()?.to_string(),
      archive_marks,
    };
    Some((checkpoint, newline + 1))
  }
}

/// The digest of the [`WINDOW_BYTES`] window that ends at `end` (or starts at 0 when that is shorter).
fn window_sha(bytes: &[u8], end: usize) -> String {
  let start = end.saturating_sub(WINDOW_BYTES);
  crate::control_events::sha256_bytes_hex(&bytes[start..end])
}

/// The event lines the checkpoint file holds: one compact JSON object per kept event, in fold order.
pub(crate) fn encode_events<'a>(events: impl IntoIterator<Item = &'a SharedChatEvent>) -> String {
  let mut text = String::new();
  for event in events {
    let value = event.to_value();
    text.push_str(&text_repr::json_dumps(&value, &CHECKPOINT_JSON));
    text.push('\n');
  }
  text
}

/// Write the checkpoint's text to `path`. The file is startup-only: a torn or stale copy fails validation and the next
/// boot scans the log whole, so the write takes no sync.
pub(crate) fn write(path: &Path, text: &str) -> StoreResult<()> {
  if let Some(parent) = path.parent() {
    std::fs::create_dir_all(parent).map_err(|error| StoreError::new(format!("{}: {error}", parent.display())))?;
  }
  fsutil::atomic_write_text(path, text).map_err(|error| StoreError::new(format!("{}: {error}", path.display())))
}
