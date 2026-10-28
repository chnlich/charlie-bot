//! The chat-events file rules: the archive file-name rule shared by the store and the disk sweep, and the first-match
//! reader that backends use to find one value in a session's live log.

use std::fs::File;
use std::io::{BufRead, BufReader, ErrorKind};
use std::path::Path;

use serde_json::Value;

use crate::{StoreError, StoreResult};

/// Whether a file name in a session's `data/archives/` directory is a chat-events archive, matching
/// Python's `src/runtime/chat_events.py:47` `ARCHIVE_FILE_GLOB` (`chat_events.*.jsonl`): a bare
/// `chat_events.jsonl` carries the live log's name, not an archive's.
pub fn is_chat_events_archive(file_name: &str) -> bool {
  file_name.strip_prefix("chat_events.").and_then(|rest| rest.strip_suffix(".jsonl")).is_some()
}

/// Walk the chat-events log at `path` in file order and return the first value that `find` maps out of an event. The
/// read stops at the line that matches. A missing file has no events, a blank or unparsable line is skipped, and a line
/// that parses to something other than a JSON object fails the read.
pub fn find_in_log<T>(path: &Path, find: impl FnMut(&Value) -> Option<T>) -> StoreResult<Option<T>> {
  let file = match File::open(path) {
    Ok(file) => file,
    Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
    Err(error) => return Err(StoreError::new(format!("{}: {error}", path.display()))),
  };
  find_in_lines(BufReader::new(file), path, find)
}

fn find_in_lines<T>(
  mut reader: impl BufRead,
  path: &Path,
  mut find: impl FnMut(&Value) -> Option<T>,
) -> StoreResult<Option<T>> {
  let mut line = Vec::new();
  loop {
    line.clear();
    let read = reader
      .read_until(b'\n', &mut line)
      .map_err(|error| StoreError::new(format!("reading chat events {}: {error}", path.display())))?;
    if read == 0 {
      return Ok(None);
    }
    if line.iter().all(u8::is_ascii_whitespace) {
      continue;
    }
    let event = match serde_json::from_slice::<Value>(&line) {
      Ok(event) => event,
      Err(error) => {
        tracing::debug!(path = %path.display(), error = %error, "chat_event_parse_skip");
        continue;
      }
    };
    if !event.is_object() {
      return Err(StoreError::new(format!("{}: chat event line is not a JSON object", path.display())));
    }
    if let Some(found) = find(&event) {
      return Ok(Some(found));
    }
  }
}

#[cfg(test)]
mod tests {
  use std::io::{self, BufRead, Cursor, Read};
  use std::path::Path;

  use serde_json::Value;

  use super::{find_in_lines, is_chat_events_archive};

  /// Counts the bytes a reader consumes, so the test can prove the walk stops at the matching line.
  struct CountingBufRead<R> {
    inner: R,
    consumed: usize,
  }

  impl<R: Read> Read for CountingBufRead<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
      let count = self.inner.read(buffer)?;
      self.consumed += count;
      Ok(count)
    }
  }

  impl<R: BufRead> BufRead for CountingBufRead<R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
      self.inner.fill_buf()
    }

    fn consume(&mut self, amount: usize) {
      self.consumed += amount;
      self.inner.consume(amount);
    }
  }

  #[test]
  fn test_archive_names_match_the_glob() {
    assert!(is_chat_events_archive("chat_events.1.jsonl"));
    assert!(is_chat_events_archive("chat_events..jsonl"));
  }

  #[test]
  fn test_non_archive_names_do_not_match_the_glob() {
    assert!(!is_chat_events_archive("chat_events.jsonl"));
    assert!(!is_chat_events_archive("chat_events.1.json"));
    assert!(!is_chat_events_archive("x.chat_events.1.jsonl"));
  }

  #[test]
  fn find_stops_after_the_first_matching_line() {
    let mut contents = b"{\"session_id\":\"thread-a\"}\n".to_vec();
    contents.extend_from_slice(&b"{}\n".repeat(1024 * 1024));
    let mut reader = CountingBufRead { inner: Cursor::new(contents), consumed: 0 };

    let id = find_in_lines(&mut reader, Path::new("chat_events.jsonl"), |event: &Value| {
      event.get("session_id").and_then(Value::as_str).map(str::to_string)
    })
    .unwrap();

    assert_eq!(id.as_deref(), Some("thread-a"));
    assert_eq!(reader.consumed, b"{\"session_id\":\"thread-a\"}\n".len());
  }
}
