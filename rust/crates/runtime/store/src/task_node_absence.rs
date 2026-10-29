//! The cheap answer to whether a session holds no task-tree node.
//!
//! A task facts fold parses a session's whole event log. A chat log that never held a task boundary has nothing to
//! fold: its task facts hold no boundary and its runs are empty, so the boot pass can only report the node absence.
//! This module reads the raw event bytes to answer that absence without parsing them.

use std::fs;
use std::path::Path;

use charliebot_core::event_types;
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
fn file_holds_task_event(path: &Path) -> StoreResult<bool> {
  let map = match charliebot_core::fsutil::map_file(path) {
    Ok(Some(map)) => map,
    Ok(None) => return Ok(false),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
    Err(error) => return Err(StoreError::new(format!("{}: {error}", path.display()))),
  };
  Ok(holds_task_event(&map))
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
            // Only an ASCII escape can spell a type name; any other code unit is written as a byte no name holds.
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

/// The value of four hex digits, or `None` when one of them is not a hex digit.
fn hex4(digits: &[u8]) -> Option<u32> {
  digits.iter().try_fold(0, |value, digit| {
    let digit = (*digit as char).to_digit(16)?;
    Some(value * 16 + digit)
  })
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
