//! The cheap answer to whether a session holds no task-tree node.
//!
//! A task facts fold parses a session's whole event log. A chat log that never held a task boundary has nothing to
//! fold. Its task facts hold no boundary and its runs are empty, so the boot pass can only report the node absence.
//! This module reads the raw event bytes to answer that absence without parsing them.

use std::fs;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;

use charliebot_core::event_types;
use charliebot_core::json_loads::hex4;
use memchr::memmem;

use crate::layout;
use crate::metadata::SessionMetadata;
use crate::{Store, StoreError, StoreResult};

/// The event types that set a task-tree boundary or a prompt fact: a history holding any of them still has a node
/// for the boot pass to reconcile.
const TASK_EVENT_TYPES: [&str; 5] = [
  event_types::TASK_CREATED,
  event_types::TASK_IMPORTED,
  event_types::TASK_CLOSED,
  event_types::TASK_REOPENED,
  event_types::PROMPT_CHANGED,
];
/// The JSON key whose string value is an event's type.
const TYPE_KEY: &[u8] = b"\"type\"";
/// The longest task event name, an upper bound on the bytes a `"type"` value needs to hold.
const MAX_EVENT_TYPE_LEN: usize = 16;
/// The bytes one read of a log brings in while [`file_holds_task_event`] looks for a type value.
///
/// A log is read in pieces of this size rather than mapped whole. The boot walk scans every candidate log of a home. A
/// mapped 138 MB log faults that much of itself into the process, which the peak RSS keeps. A piece extends to the end
/// of the line it ends inside, so the scan holds one event's line at most, never the log. Tests shrink the piece, so a
/// line that crosses pieces is written in a few bytes.
#[cfg(not(test))]
const SCAN_CHUNK_BYTES: usize = 1 << 20;
#[cfg(test)]
const SCAN_CHUNK_BYTES: usize = 64;

/// The error a task read returns for a session whose history holds no creation or import boundary.
pub fn not_a_task_tree_node(session_id: &str) -> StoreError {
  StoreError::invalid(format!("task {session_id} has no creation or import boundary; it is not a task-tree node"))
}

impl Store {
  /// Whether this session holds no task-tree node: no boundary or prompt event in its event logs, no run directory,
  /// and no prompt reference on its metadata. The boot pass then has nothing to reconcile for it.
  pub fn has_no_task_node(&self, meta: &SessionMetadata) -> StoreResult<bool> {
    if meta.subtree_prompt_ref.is_some() || meta.node_prompt_ref.is_some() {
      return Ok(false);
    }
    if self.run_directory_exists(&meta.id)? {
      return Ok(false);
    }
    Ok(!self.history_holds_task_event(&meta.id)?)
  }

  /// Whether any event log of the session names a task boundary or prompt-change event. The live log goes first: a
  /// node's creation fact stands at its head, so a node answers after one short read.
  fn history_holds_task_event(&self, session_id: &str) -> StoreResult<bool> {
    if file_holds_task_event(&layout::chat_events_path(&self.session_dir(session_id)))? {
      return Ok(true);
    }
    for path in self.archive_paths(session_id)?.iter() {
      if file_holds_task_event(path)? {
        return Ok(true);
      }
    }
    Ok(false)
  }

  /// Whether the session's run directory holds an entry the run listing would read.
  fn run_directory_exists(&self, session_id: &str) -> StoreResult<bool> {
    let root = self.runs_root(session_id);
    let entries = match fs::read_dir(&root) {
      Ok(entries) => entries,
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
      Err(error) => return Err(StoreError::new(format!("{}: {error}", root.display()))),
    };
    for entry in entries {
      let entry = entry.map_err(|error| StoreError::new(format!("{}: {error}", root.display())))?;
      let path = entry.path();
      if entry.file_type().map_err(|error| StoreError::new(format!("{}: {error}", path.display())))?.is_dir() {
        return Ok(true);
      }
    }
    Ok(false)
  }
}

/// Whether one event log names a task boundary or prompt-change event. A missing file holds none.
///
/// The log holds one JSON object per line, so a piece ends on a line boundary. A piece extends to the end of the line
/// it ends inside, and the next piece starts after the piece's last newline. A `"type"` key and its value are therefore
/// always read in one piece. That holds however much whitespace the writer put between the key, its colon and its
/// value.
fn file_holds_task_event(path: &Path) -> StoreResult<bool> {
  let file = match File::open(path) {
    Ok(file) => file,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
    Err(error) => return Err(StoreError::new(format!("{}: {error}", path.display()))),
  };
  // A log shorter than a piece has one read, so its buffer is the log's own length. Most logs of a home are a few
  // hundred bytes. Zeroing a piece for each would clear far more than they hold.
  let length = match file.metadata() {
    Ok(metadata) => usize::try_from(metadata.len()).expect("a log's length fits in usize"),
    Err(error) => return Err(StoreError::new(format!("{}: {error}", path.display()))),
  };
  let mut piece: Vec<u8> = Vec::new();
  let mut offset = 0usize;
  while offset < length {
    piece.resize(SCAN_CHUNK_BYTES.min(length - offset), 0);
    let mut read = read_into(&file, path, &mut piece, offset)?;
    if read == 0 {
      return Ok(false);
    }
    piece.truncate(read);
    // A line longer than a piece is read to its end, so no key is answered from a piece that stops inside it.
    while offset + read < length && !piece[..read].contains(&b'\n') {
      piece.resize(read + SCAN_CHUNK_BYTES.min(length - offset - read), 0);
      let more = read_into(&file, path, &mut piece[read..], offset + read)?;
      if more == 0 {
        piece.truncate(read);
        break;
      }
      read += more;
      piece.truncate(read);
    }
    // A piece that ends inside the log is scanned up to its last newline. A writer that truncated the log under the
    // read leaves no such line, and then the piece's bytes are all the scan has.
    let end = if offset + read == length {
      read
    } else {
      piece[..read].iter().rposition(|byte| *byte == b'\n').map_or(read, |at| at + 1)
    };
    if holds_task_event(&piece[..end]) {
      return Ok(true);
    }
    offset += end;
  }
  Ok(false)
}

/// The bytes of `path` at `offset` into `buffer`, up to the buffer's length.
fn read_into(file: &File, path: &Path, buffer: &mut [u8], offset: usize) -> StoreResult<usize> {
  let at = u64::try_from(offset).expect("a log's offset fits in u64");
  file.read_at(buffer, at).map_err(|error| StoreError::new(format!("{}: {error}", path.display())))
}

/// Whether any `"type"` value names a task boundary or prompt-change event. The value is read as a JSON string, so an
/// escaped spelling (`task_created`) counts, as it does for the fold that parses the event.
fn holds_task_event(bytes: &[u8]) -> bool {
  let mut value = [0u8; MAX_EVENT_TYPE_LEN];
  memmem::find_iter(bytes, TYPE_KEY).any(|at| {
    let Some(len) = type_value(&bytes[at + TYPE_KEY.len()..], &mut value) else {
      return false;
    };
    TASK_EVENT_TYPES.iter().any(|event_type| value[..len] == event_type.as_bytes()[..])
  })
}

/// The string value that follows a `"type"` key: the colon, then a JSON string decoded into `value`, whose length is
/// returned. A value longer than `value`, unterminated, or not a string is absent.
fn type_value(rest: &[u8], value: &mut [u8; MAX_EVENT_TYPE_LEN]) -> Option<usize> {
  let rest = trim_space(rest).strip_prefix(b":")?;
  let rest = trim_space(rest).strip_prefix(b"\"")?;
  let mut len = 0;
  let mut index = 0;
  while index < rest.len() {
    let byte = match rest[index] {
      b'"' => return Some(len),
      b'\\' => {
        index += 1;
        match rest.get(index)? {
          b'"' => b'"',
          b'\\' => b'\\',
          b'/' => b'/',
          b'b' => 0x08,
          b'f' => 0x0c,
          b'n' => b'\n',
          b'r' => b'\r',
          b't' => b'\t',
          b'u' => {
            let code = hex4(rest.get(index + 1..index + 5)?)?;
            index += 4;
            // Only an ASCII escape can spell a type name. Any other code unit is written as a byte no name holds.
            u8::try_from(code).unwrap_or(0xff)
          }
          _ => return None,
        }
      }
      byte => byte,
    };
    *value.get_mut(len)? = byte;
    len += 1;
    index += 1;
  }
  None
}

/// The bytes before the first byte that is not JSON structural whitespace.
fn trim_space(mut bytes: &[u8]) -> &[u8] {
  while let Some((first, rest)) = bytes.split_first() {
    if !matches!(first, b' ' | b'\t' | b'\r' | b'\n') {
      break;
    }
    bytes = rest;
  }
  bytes
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::Store;
  use crate::metadata::TaskProfile;

  fn write_imported_chat(store: &Store, session_id: &str, events: &str) -> SessionMetadata {
    let dir = store.session_dir(session_id);
    fs::create_dir_all(layout::data_dir(&dir)).unwrap();
    let meta = SessionMetadata::new(session_id, "Imported chat", TaskProfile::Manager);
    fs::write(layout::metadata_path(&dir), meta.to_json_string(&store.slots, true)).unwrap();
    fs::write(layout::chat_events_path(&dir), events).unwrap();
    meta
  }

  #[test]
  fn a_type_value_names_a_task_event_however_it_is_spelled() {
    assert!(holds_task_event(br#"{"id": "e1", "type": "task_created", "actor": "user"}"#));
    assert!(holds_task_event(br#"{"type":"task_closed"}"#));
    assert!(holds_task_event(br#"{"type": "prompt_changed", "scope": "node"}"#));
    assert!(holds_task_event(br#"{"type" : "task_reopened"}"#));
    assert!(holds_task_event(br#"{"type": "task_imported"}"#));
    // An escaped spelling parses to the same type, so the scan reads the value rather than the raw bytes.
    assert!(holds_task_event("{\"type\": \"task\\u005fcreated\"}".as_bytes()));
    assert!(holds_task_event("{\"type\": \"user\"}\n{\"type\": \"task_closed\"}".as_bytes()));
    assert!(!holds_task_event(br#"{"id": "e1", "type": "user", "content": "task list"}"#));
    assert!(!holds_task_event(br#"{"type": "assistant", "content": "the task_runner is slow"}"#));
    assert!(!holds_task_event(br#"{"type": "task_closer"}"#));
    assert!(!holds_task_event(br#"{"type_hint": "task_created"}"#));
    assert!(!holds_task_event(br#"{"type": "task_created"#));
  }

  #[test]
  fn a_type_value_is_found_wherever_its_line_meets_a_piece() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chat_events.jsonl");
    // The first read ends at byte SCAN_CHUNK_BYTES, so pad residue r puts that end SCAN_CHUNK_BYTES - r bytes into the
    // 39-byte event line (newline included). With SCAN_CHUNK_BYTES = 64, residues 28..=63 put it 1..=36 bytes in, from
    // before the key up to the type value's closing quote at byte 36. Residues 26 and 27 put it after that quote.
    for pad in 28..=63 {
      let mut boundary = vec![b'\n'; pad];
      boundary.extend_from_slice(b"{\"id\": \"e1\", \"type\": \"prompt_changed\"}\n");
      fs::write(&path, &boundary).unwrap();
      assert!(file_holds_task_event(&path).unwrap(), "a task event on the line after {pad} empty ones is a boundary");

      let mut ordinary = vec![b'\n'; pad];
      ordinary.extend_from_slice(b"{\"id\": \"e1\", \"type\": \"user\", \"content\": \"prompt_changed\"}\n");
      fs::write(&path, &ordinary).unwrap();
      assert!(!file_holds_task_event(&path).unwrap(), "an ordinary event after {pad} empty ones holds none");
    }
  }

  #[test]
  fn a_type_value_past_a_piece_inside_one_line_is_found() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chat_events.jsonl");
    // One line runs past three pieces, and its `"type"` value is written after the first piece's end.
    let mut boundary = b"{\"id\": \"e1\", \"content\": \"".to_vec();
    boundary.resize(boundary.len() + 3 * SCAN_CHUNK_BYTES, b'x');
    boundary.extend_from_slice(b"\", \"type\": \"task_closed\"}\n");
    fs::write(&path, &boundary).unwrap();
    assert!(file_holds_task_event(&path).unwrap(), "a type value past the first piece of a long line is read");

    let mut ordinary = b"{\"id\": \"e1\", \"content\": \"".to_vec();
    ordinary.resize(ordinary.len() + 3 * SCAN_CHUNK_BYTES, b'x');
    ordinary.extend_from_slice(b"\", \"type\": \"user\", \"note\": \"task_closed\"}\n");
    fs::write(&path, &ordinary).unwrap();
    assert!(!file_holds_task_event(&path).unwrap(), "an ordinary long line holds no task event");
  }

  #[test]
  fn an_imported_chat_log_holds_no_task_node() {
    let home = tempfile::tempdir().unwrap();
    let store = crate::task_tree::tests::make_store(home.path());
    let meta =
      write_imported_chat(&store, "imported-chat", "{\"id\": \"e1\", \"type\": \"user\", \"content\": \"hello\"}\n");
    assert!(store.has_no_task_node(&meta).unwrap());
  }

  #[test]
  fn a_boundary_event_in_the_live_log_keeps_the_node() {
    let home = tempfile::tempdir().unwrap();
    let store = crate::task_tree::tests::make_store(home.path());
    let meta = write_imported_chat(
      &store,
      "created-task",
      "{\"id\": \"e1\", \"type\": \"task_created\", \"actor\": \"user\"}\n",
    );
    assert!(!store.has_no_task_node(&meta).unwrap());
  }

  #[test]
  fn a_boundary_event_in_an_archive_keeps_the_node() {
    let home = tempfile::tempdir().unwrap();
    let store = crate::task_tree::tests::make_store(home.path());
    let meta =
      write_imported_chat(&store, "archived-task", "{\"id\": \"e2\", \"type\": \"user\", \"content\": \"hi\"}\n");
    let archives = layout::archives_dir(&store.session_dir(&meta.id));
    fs::create_dir_all(&archives).unwrap();
    fs::write(archives.join("chat_events.1.jsonl"), "{\"id\": \"e1\", \"type\": \"task_closed\"}\n").unwrap();
    assert!(!store.has_no_task_node(&meta).unwrap());
  }

  #[test]
  fn a_run_directory_keeps_the_node() {
    let home = tempfile::tempdir().unwrap();
    let store = crate::task_tree::tests::make_store(home.path());
    let meta = write_imported_chat(&store, "with-runs", "{\"id\": \"e1\", \"type\": \"user\", \"content\": \"hi\"}\n");
    fs::create_dir_all(store.run_dir(&meta.id, "run-1")).unwrap();
    assert!(!store.has_no_task_node(&meta).unwrap());
  }

  #[test]
  fn a_prompt_reference_on_the_metadata_keeps_the_node() {
    let home = tempfile::tempdir().unwrap();
    let store = crate::task_tree::tests::make_store(home.path());
    let mut meta =
      write_imported_chat(&store, "prompted", "{\"id\": \"e1\", \"type\": \"user\", \"content\": \"hi\"}\n");
    meta.node_prompt_ref = Some("prompt-body-1".to_string());
    assert!(!store.has_no_task_node(&meta).unwrap());
  }
}
