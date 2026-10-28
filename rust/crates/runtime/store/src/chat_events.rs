//! The chat-events file rules. The live log `data/chat_events.jsonl` and its archives are one file kind: this module
//! owns their names, the line rules that split them, the one decoder for a line, the append of one event, and the
//! readers that the other crates call. `find_in_log` is the first-match reader that backends use to find one value in a
//! session's live log.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, ErrorKind};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::layout;
use crate::ndjson;
use crate::{StoreError, StoreResult};

/// The message `read_to_string` gives for bytes that are not UTF-8.
const INVALID_UTF8_MESSAGE: &str = "stream did not contain valid UTF-8";

/// Whether a file name in a session's `data/archives/` directory is a chat-events archive, matching
/// Python's `src/runtime/chat_events.py:47` `ARCHIVE_FILE_GLOB` (`chat_events.*.jsonl`): a bare
/// `chat_events.jsonl` carries the live log's name, not an archive's.
pub fn is_chat_events_archive(file_name: &str) -> bool {
  file_name.strip_prefix("chat_events.").and_then(|rest| rest.strip_suffix(".jsonl")).is_some()
}

/// How a chat event log splits into lines and how its bytes become text. A reader names one rule for each read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineRule {
  /// `str::lines` over the whole file, read as strict UTF-8: an invalid byte fails the read before any line is read.
  Lines,
  /// `str::lines` over the whole file, read lossily: each invalid byte becomes U+FFFD.
  LossyLines,
  /// `str::lines`, each line read on its own: a line that is not UTF-8, or fails to read, fails at that line, after
  /// the lines before it.
  PerLine,
  /// `split_inclusive('\n')` over the whole file, read as strict UTF-8: each line keeps its `\n`.
  Terminated,
  /// Python universal newlines over the whole file, read as strict UTF-8: `\r\n` and a lone `\r` end a line as `\n`
  /// does, and a final line without a terminator is a line.
  Universal,
}

/// One line of a chat event log, as the line rule of its read split it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogLine {
  /// The line's 1-based number in its file, blank lines included.
  pub number: usize,
  /// The line's text. A `Terminated` line keeps its `\n`; the other rules drop the terminator.
  pub text: String,
}

impl LogLine {
  /// Decode the line as one JSON value. A value that is not an object is for the caller to refuse or skip.
  pub fn decode(&self) -> Result<Value, serde_json::Error> {
    decode_line(self.text.as_bytes())
  }
}

/// The one decoder for a chat event line: its JSON value, with the whitespace a line may carry around it.
pub fn decode_line(line: &[u8]) -> Result<Value, serde_json::Error> {
  serde_json::from_slice(line)
}

/// Why a chat event log could not be read as lines.
#[derive(Debug)]
pub enum LogReadError {
  /// The file could not be opened or read as a whole. A `NotFound` error means the log is missing.
  Io(io::Error),
  /// Line `number` could not be read. Only the `PerLine` rule reads a line at a time.
  Line { number: usize, error: io::Error },
  /// The file is not UTF-8 under a strict rule. The error keeps the bytes, so a caller can name the first bad byte.
  Utf8(std::string::FromUtf8Error),
}

impl LogReadError {
  /// The I/O kind of the failure. Bytes that are not UTF-8 are `InvalidData`, as `read_to_string` reports them.
  pub fn kind(&self) -> ErrorKind {
    match self {
      Self::Io(error) | Self::Line { error, .. } => error.kind(),
      Self::Utf8(_) => ErrorKind::InvalidData,
    }
  }
}

impl fmt::Display for LogReadError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Io(error) | Self::Line { error, .. } => fmt::Display::fmt(error, f),
      Self::Utf8(_) => f.write_str(INVALID_UTF8_MESSAGE),
    }
  }
}

impl std::error::Error for LogReadError {}

/// The lines of one read, in file order. A whole-file rule fails before its first line; `PerLine` fails at the line.
pub type LogLines = Box<dyn Iterator<Item = Result<LogLine, LogReadError>>>;

/// The lines of the chat event log at `path`, split and read by `rule`. A missing file is `LogReadError::Io` with
/// `NotFound`; the caller decides whether that means an empty log.
pub fn read_lines(path: &Path, rule: LineRule) -> Result<LogLines, LogReadError> {
  match rule {
    LineRule::PerLine => {
      let file = File::open(path).map_err(LogReadError::Io)?;
      let mut number = 0;
      Ok(Box::new(BufReader::new(file).lines().map(move |line| {
        number += 1;
        line.map(|text| LogLine { number, text }).map_err(|error| LogReadError::Line { number, error })
      })))
    }
    LineRule::Lines | LineRule::LossyLines | LineRule::Terminated | LineRule::Universal => {
      let bytes = fs::read(path).map_err(LogReadError::Io)?;
      let text = match rule {
        LineRule::LossyLines => String::from_utf8_lossy(&bytes).into_owned(),
        _ => String::from_utf8(bytes).map_err(LogReadError::Utf8)?,
      };
      let lines = split_whole(&text, rule);
      Ok(Box::new(lines.into_iter().enumerate().map(|(index, text)| Ok(LogLine { number: index + 1, text }))))
    }
  }
}

/// Split a whole file's text under a rule that reads the file whole.
fn split_whole(text: &str, rule: LineRule) -> Vec<String> {
  match rule {
    LineRule::Lines | LineRule::LossyLines => text.lines().map(str::to_string).collect(),
    LineRule::Terminated => text.split_inclusive('\n').map(str::to_string).collect(),
    LineRule::Universal => {
      let normalized = read_universal(text);
      normalized.split_terminator('\n').map(str::to_string).collect()
    }
    LineRule::PerLine => unreachable!("the per-line rule reads the file one line at a time, not as one text"),
  }
}

/// The text of a live or archived log with Python's universal newlines: `\r\n` and a lone `\r` end a line as `\n`
/// does. A reader that polls a growing log calls this on each read and passes the result to `take_complete_lines`.
pub fn read_universal_text(path: &Path) -> Result<String, LogReadError> {
  let bytes = fs::read(path).map_err(LogReadError::Io)?;
  let text = String::from_utf8(bytes).map_err(LogReadError::Utf8)?;
  Ok(read_universal(&text))
}

fn read_universal(text: &str) -> String {
  text.replace("\r\n", "\n").replace('\r', "\n")
}

/// The complete lines of `text` after byte `consumed`, each numbered after the `lines` already taken, and advance
/// `consumed` and `lines` past them. A line still being written has no `\n` and stays unread, so the next call, on
/// the grown text, returns it whole.
pub fn take_complete_lines(text: &str, consumed: &mut usize, lines: &mut usize) -> Vec<LogLine> {
  let mut taken = Vec::new();
  while let Some(offset) = text[*consumed..].find('\n') {
    let end = *consumed + offset;
    *lines += 1;
    taken.push(LogLine { number: *lines, text: text[*consumed..end].to_string() });
    *consumed = end + 1;
  }
  taken
}

/// The archive files of a session, in file-name order. A session without an archives directory has none.
pub fn archive_files(session_dir: &Path) -> io::Result<Vec<PathBuf>> {
  let dir = layout::archives_dir(session_dir);
  let entries = match fs::read_dir(&dir) {
    Ok(entries) => entries,
    Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
    Err(error) => return Err(error),
  };
  let mut entries = entries.collect::<io::Result<Vec<_>>>()?;
  entries.sort_by_key(|entry| entry.file_name());
  Ok(
    entries
      .into_iter()
      .filter(|entry| is_chat_events_archive(&entry.file_name().to_string_lossy()))
      .map(|entry| entry.path())
      .collect(),
  )
}

/// The chat event logs of a session in reading order: each archive in file-name order, then the live log when it is
/// a file. A `data/archives` that is not a directory holds no archives; the live log still counts.
pub fn session_log_paths(session_dir: &Path) -> io::Result<Vec<PathBuf>> {
  let mut paths = if layout::archives_dir(session_dir).is_dir() { archive_files(session_dir)? } else { Vec::new() };
  let live = layout::chat_events_path(session_dir);
  if live.is_file() {
    paths.push(live);
  }
  Ok(paths)
}

/// Append one chat event as a line and sync it to disk. The file and its directory are created when absent.
pub fn append_event(path: &Path, event: &Value) -> StoreResult<()> {
  ndjson::append(path, event)
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
    let event = match decode_line(&line) {
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

  use super::{LineRule, LogLine, LogReadError, decode_line, find_in_lines, is_chat_events_archive};

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

  /// A log file in a fresh temporary directory, holding `bytes`.
  fn log_with(bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let path = dir.path().join("chat_events.jsonl");
    std::fs::write(&path, bytes).expect("write the log");
    (dir, path)
  }

  /// The lines of a read under `rule`, or the error that the read fails with before its first line.
  fn texts(path: &Path, rule: LineRule) -> Result<Vec<(usize, String)>, LogReadError> {
    let lines = super::read_lines(path, rule)?;
    lines.map(|line| line.map(|line| (line.number, line.text))).collect()
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
  fn lines_rule_drops_the_terminator_and_counts_blank_lines() {
    let (_dir, path) = log_with(b"a\r\n\nb");
    let lines = texts(&path, LineRule::Lines).unwrap();
    assert_eq!(lines, vec![(1, "a".to_string()), (2, String::new()), (3, "b".to_string())]);
  }

  #[test]
  fn lines_rule_fails_before_any_line_on_invalid_utf8() {
    let (_dir, path) = log_with(b"{}\n\xff\n");
    let error = texts(&path, LineRule::Lines).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(error.to_string(), "stream did not contain valid UTF-8");
  }

  #[test]
  fn lossy_lines_rule_replaces_each_invalid_byte() {
    let (_dir, path) = log_with(b"ok\n\xff\n");
    let lines = texts(&path, LineRule::LossyLines).unwrap();
    assert_eq!(lines, vec![(1, "ok".to_string()), (2, "\u{fffd}".to_string())]);
  }

  #[test]
  fn per_line_rule_fails_at_the_bad_line_after_the_lines_before_it() {
    let (_dir, path) = log_with(b"{\"a\":1}\n\xff\n{\"b\":2}\n");
    let mut lines = super::read_lines(&path, LineRule::PerLine).unwrap();
    assert_eq!(lines.next().unwrap().unwrap().text, "{\"a\":1}");
    let error = lines.next().unwrap().unwrap_err();
    assert!(matches!(error, LogReadError::Line { number: 2, .. }), "the second line fails: {error:?}");
  }

  #[test]
  fn terminated_rule_keeps_each_newline() {
    let (_dir, path) = log_with(b"a\nb");
    let lines = texts(&path, LineRule::Terminated).unwrap();
    assert_eq!(lines, vec![(1, "a\n".to_string()), (2, "b".to_string())]);
  }

  #[test]
  fn universal_rule_ends_a_line_at_a_lone_cr_and_keeps_a_final_line() {
    let (_dir, path) = log_with(b"a\rb\r\nc");
    let lines = texts(&path, LineRule::Universal).unwrap();
    assert_eq!(lines, vec![(1, "a".to_string()), (2, "b".to_string()), (3, "c".to_string())]);
  }

  #[test]
  fn take_complete_lines_returns_each_line_once_and_leaves_the_partial_tail() {
    let mut consumed = 0;
    let mut lines = 0;
    let first = super::take_complete_lines("x\ny", &mut consumed, &mut lines);
    assert_eq!(first, vec![LogLine { number: 1, text: "x".to_string() }]);
    let second = super::take_complete_lines("x\nyz\n", &mut consumed, &mut lines);
    assert_eq!(second, vec![LogLine { number: 2, text: "yz".to_string() }]);
    assert!(super::take_complete_lines("x\nyz\n", &mut consumed, &mut lines).is_empty());
  }

  #[test]
  fn read_universal_text_converts_newlines_and_fails_on_invalid_utf8() {
    let (_dir, path) = log_with(b"a\r\nb\rc\n");
    assert_eq!(super::read_universal_text(&path).unwrap(), "a\nb\nc\n");
    let (_bad_dir, bad) = log_with(b"\xff");
    let error = super::read_universal_text(&bad).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
  }

  #[test]
  fn archive_files_are_in_file_name_order_and_the_live_log_comes_last() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let session = dir.path();
    let archives = session.join("data").join("archives");
    std::fs::create_dir_all(&archives).unwrap();
    std::fs::write(session.join("data").join("chat_events.jsonl"), "{}\n").unwrap();
    for name in ["chat_events.2026-W02.jsonl", "chat_events.2026-W01.jsonl", "notes.txt"] {
      std::fs::write(archives.join(name), "{}\n").unwrap();
    }

    let names: Vec<String> = super::archive_files(session)
      .unwrap()
      .iter()
      .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
      .collect();
    assert_eq!(names, ["chat_events.2026-W01.jsonl", "chat_events.2026-W02.jsonl"]);

    let paths = super::session_log_paths(session).unwrap();
    assert_eq!(paths.len(), 3);
    assert_eq!(paths.last().unwrap(), &session.join("data").join("chat_events.jsonl"));
  }

  #[test]
  fn session_log_paths_skips_an_archives_path_that_is_not_a_directory() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let session = dir.path();
    std::fs::create_dir_all(session.join("data")).unwrap();
    std::fs::write(session.join("data").join("archives"), "not a directory").unwrap();
    std::fs::write(session.join("data").join("chat_events.jsonl"), "{}\n").unwrap();

    let paths = super::session_log_paths(session).unwrap();
    assert_eq!(paths, vec![session.join("data").join("chat_events.jsonl")]);
  }

  #[test]
  fn decode_line_takes_a_bare_or_terminated_line_and_rejects_a_blank_one() {
    assert_eq!(decode_line(b"{\"a\":1}").unwrap(), serde_json::json!({"a": 1}));
    assert_eq!(decode_line(b"{\"a\":1}\n").unwrap(), serde_json::json!({"a": 1}));
    assert!(decode_line(b"").is_err());
  }

  #[test]
  fn append_event_writes_one_python_style_line() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let path = dir.path().join("data").join("chat_events.jsonl");
    super::append_event(&path, &serde_json::json!({"a": 1})).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"a\": 1}\n");
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

  /// The store's two chat line decoders — this module's `decode_line` and the compact mmap fast path's
  /// `parse_line_compact` — agree on every line both can read, and differ today only on bytes that are not
  /// UTF-8: the strict decode fails them and the compact decode repairs them to U+FFFD first. A change to one
  /// decoder fails here until the other follows.
  #[test]
  fn compact_and_value_line_decoders_agree_over_the_line_corpus() {
    use crate::ndjson::parse_line_compact;

    // A blank line, a whitespace line and a malformed line reject under both decoders.
    for line in [b"".as_slice(), b"   ".as_slice(), b"{broken".as_slice()] {
      assert!(decode_line(line).is_err(), "{line:?}");
      assert_eq!(parse_line_compact(line), None, "{line:?}");
    }
    // A nested value, and a line that keeps its `\n` terminator, decode to the same value.
    for line in [
      br#"{"b": 1, "a": {"deep": [1, 2.5, null, true, {"x": "y"}]}}"#.as_slice(),
      br#"{"a": 1}
"#
      .as_slice(),
    ] {
      assert_eq!(decode_line(line).unwrap(), parse_line_compact(line).unwrap().to_value(), "{line:?}");
    }
    // The difference of today.
    assert!(decode_line(b"{\"text\": \"ok\xff\"}").is_err());
    assert_eq!(
      parse_line_compact(b"{\"text\": \"ok\xff\"}").map(|compact| compact.to_value()),
      Some(serde_json::json!({"text": "ok\u{fffd}"}))
    );
  }
}
