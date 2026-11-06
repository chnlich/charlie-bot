use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use charliebot_core::fsutil;
use charliebot_core::text_repr::{self, JsonStyle};
use memchr::memchr_iter;
use serde::de::DeserializeSeed;
use serde_json::Value;

use crate::compact::{CompactValue, ParseKeys, ValueSeed};
use crate::run_logs;
use crate::{StoreError, StoreResult};

const APPEND_JSON: JsonStyle =
  JsonStyle { ensure_ascii: false, sort_keys: false, item_separator: ", ", key_separator: ": ", indent: None };

/// Parse one NDJSON line. Blank and malformed lines are skipped. Invalid UTF-8 is replaced with U+FFFD before
/// the parse, so such a line can still parse. A final partial line is parsed just like any other line.
///
/// One pass: the line deserializes straight into [`CompactValue`] — the reader's only event form —
/// through the visitor in `compact`. That visitor produces the same value a `serde_json::Value`
/// round trip would have.
pub(crate) fn parse_line_compact(raw: &[u8]) -> Option<CompactValue> {
  parse_line(raw, |text| serde_json::from_str(text).ok())
}

/// [`parse_line_compact`] with the object keys taken from one file's `keys`.
pub(crate) fn parse_file_line(raw: &[u8], keys: &ParseKeys) -> Option<CompactValue> {
  parse_line(raw, |text| parse_file_text(text, keys))
}

/// One line's text parsed with the object keys taken from one file's `keys`; `None` for malformed JSON.
pub(crate) fn parse_file_text(text: &str, keys: &ParseKeys) -> Option<CompactValue> {
  // `serde_json::from_str` is this deserialize followed by the check that only whitespace remains.
  let mut deserializer = serde_json::Deserializer::from_str(text);
  let value = ValueSeed(keys).deserialize(&mut deserializer).ok()?;
  deserializer.end().ok()?;
  Some(value)
}

/// The skip contract of one line: a blank line is `None`, and `parse` reads the line's text, with invalid UTF-8 replaced
/// by U+FFFD.
pub(crate) fn parse_line(raw: &[u8], parse: impl Fn(&str) -> Option<CompactValue>) -> Option<CompactValue> {
  if raw.iter().all(u8::is_ascii_whitespace) {
    return None;
  }
  #[cfg(test)]
  LINE_PARSES.with(|count| count.set(count.get() + 1));
  // `from_utf8` validates word-at-a-time, and `from_utf8_lossy` walks byte by byte even over valid text. A valid line,
  // which a lossy decode returns unchanged, takes the fast check.
  match std::str::from_utf8(raw) {
    Ok(text) => parse(text),
    Err(_) => parse(&String::from_utf8_lossy(raw)),
  }
}

// How many lines this thread's parser attempted, a test-only count that proves a reader parses each
// line once. Thread-local because tests run in parallel and the read runs on its caller's thread.
#[cfg(test)]
thread_local! {
  static LINE_PARSES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// The lines this thread's parsers attempted since the last [`reset_line_parses`]: a test proves that a read parses
/// only the bytes it must.
#[cfg(test)]
pub(crate) fn line_parses() -> u64 {
  LINE_PARSES.with(std::cell::Cell::get)
}

/// Zero the line-parse count of this thread.
#[cfg(test)]
pub(crate) fn reset_line_parses() {
  LINE_PARSES.with(|count| count.set(0));
}

/// Parse every physical line into the compact event form, including a final line without `\n`.
///
/// The file maps once and memchr supplies the line boundaries (an mmap plus zero-copy line slices); no
/// per-line buffer is allocated.
pub(crate) fn read_file_compact(path: &Path) -> StoreResult<Vec<CompactValue>> {
  read_file_lines(path, parse_file_line)
}

/// Parse every physical line unless `should_stop` requests cancellation between lines.
///
/// A cancelled read returns no events so callers cannot publish a partial event cache.
pub(crate) fn read_file_compact_until(
  path: &Path,
  should_stop: &mut impl FnMut() -> bool,
) -> StoreResult<Option<Vec<CompactValue>>> {
  Ok(read_file_compact_sized_until(path, should_stop)?.0)
}

/// [`read_file_compact_until`] with the bytes the read opened: the file's length at open, 0 for a missing or empty
/// file or a read stopped before the open. The length comes from the open itself, so it costs no further file call.
pub(crate) fn read_file_compact_sized_until(
  path: &Path,
  should_stop: &mut impl FnMut() -> bool,
) -> StoreResult<(Option<Vec<CompactValue>>, u64)> {
  if should_stop() {
    return Ok((None, 0));
  }
  let map = match open_log_map(path)? {
    Some(map) => map,
    None => return Ok((Some(Vec::new()), 0)),
  };
  Ok((parse_mapped_lines(&map, should_stop), map.len() as u64))
}

/// Read every physical line of `path` through `line`, including a final line without `\n`; a line that `line` answers
/// with `None` adds no event. The lines share one [`ParseKeys`], as a whole-file parse's lines do.
pub(crate) fn read_file_lines(
  path: &Path,
  line: impl Fn(&[u8], &ParseKeys) -> Option<CompactValue>,
) -> StoreResult<Vec<CompactValue>> {
  let Some(map) = open_log_map(path)? else {
    return Ok(Vec::new());
  };
  Ok(lines_of(&map, &line))
}

/// When a split read splits a log's lines across threads: from `min_bytes`, into runs of at least `chunk_bytes`, on at
/// most `max_threads` threads.
struct SplitSizes {
  min_bytes: usize,
  chunk_bytes: usize,
  max_threads: usize,
}

/// The boot walk's split sizes.
///
/// A thread start takes the process's mapping lock for writing, behind every page fault of the logs mapped beside it,
/// so a split pays only for the few logs that would end the walk's scan phase alone: on the 8,100-session lab home
/// three logs of 600 reach the minimum.
const BOOT_SPLIT: SplitSizes = SplitSizes { min_bytes: 32 << 20, chunk_bytes: 16 << 20, max_threads: 4 };

/// [`read_file_lines`] with a large log's lines split across threads at the boot walk's sizes.
///
/// Each thread reads a run of whole lines with its own [`ParseKeys`] (a key compares by name, so the tables mix), and
/// the events keep the file's line order. The boot walk scans every node's log in parallel, so its largest log would
/// otherwise end the scan phase alone.
pub(crate) fn read_file_lines_split(
  path: &Path,
  line: impl Fn(&[u8], &ParseKeys) -> Option<CompactValue> + Sync,
) -> StoreResult<Vec<CompactValue>> {
  let Some(map) = open_log_map(path)? else {
    return Ok(Vec::new());
  };
  Ok(split_lines_of_sized(&map, &line, &BOOT_SPLIT))
}

/// Every line of `bytes`, split across threads at the boot walk's sizes. A caller that already holds a log's mapping
/// (a scan that resumes from a checkpoint) parses the bytes past its offset this way, with the same line rule and the
/// same order a whole-file read gives.
pub(crate) fn split_lines_of(
  bytes: &[u8],
  line: &(impl Fn(&[u8], &ParseKeys) -> Option<CompactValue> + Sync),
) -> Vec<CompactValue> {
  split_lines_of_sized(bytes, line, &BOOT_SPLIT)
}

/// [`lines_of`] with the lines of `bytes` split across threads at `sizes`.
fn split_lines_of_sized(
  bytes: &[u8],
  line: &(impl Fn(&[u8], &ParseKeys) -> Option<CompactValue> + Sync),
  sizes: &SplitSizes,
) -> Vec<CompactValue> {
  let threads = (bytes.len() / sizes.chunk_bytes).min(sizes.max_threads);
  if bytes.len() < sizes.min_bytes || threads < 2 {
    return lines_of(bytes, line);
  }
  // Each run of lines ends just after a newline, so every line lies whole in one run.
  let mut bounds = vec![0];
  for part in 1..threads {
    let target = (bytes.len() * part / threads).max(*bounds.last().expect("the bounds start at zero"));
    match memchr::memchr(b'\n', &bytes[target..]) {
      Some(offset) if target + offset + 1 < bytes.len() => bounds.push(target + offset + 1),
      _ => break,
    }
  }
  bounds.push(bytes.len());
  let runs: Vec<Vec<CompactValue>> = std::thread::scope(|scope| {
    let handles: Vec<_> =
      bounds.windows(2).map(|run| scope.spawn(move || lines_of(&bytes[run[0]..run[1]], line))).collect();
    handles.into_iter().map(|handle| handle.join().expect("a split read thread completes")).collect()
  });
  runs.into_iter().flatten().collect()
}

/// Every physical line of `bytes` through `line`, including a final line without `\n`, with one [`ParseKeys`].
fn lines_of(bytes: &[u8], line: &impl Fn(&[u8], &ParseKeys) -> Option<CompactValue>) -> Vec<CompactValue> {
  let keys = ParseKeys::default();
  let mut events = Vec::new();
  let mut start = 0;
  for boundary in memchr_iter(b'\n', bytes) {
    events.extend(line(&bytes[start..boundary], &keys));
    start = boundary + 1;
  }
  if start < bytes.len() {
    events.extend(line(&bytes[start..], &keys));
  }
  events
}

/// Read or map one event log through `run_logs`, so a compressed log reads as its decompressed bytes; `None` covers
/// a missing log and a log with no bytes.
fn open_log_map(path: &Path) -> StoreResult<Option<fsutil::FileBytes>> {
  run_logs::read_bytes(path).map_err(|error| StoreError::new(format!("{}: {error}", path.display())))
}

/// Parse every `\n`-bounded slice plus the final fragment, checking `should_stop` before each parse and once
/// after the last one, which is the check order `BufReader::split` had. A blank or malformed line skips.
/// `None` marks a cancelled read. The lines share one [`ParseKeys`], so parses of different files run in parallel.
fn parse_mapped_lines(bytes: &[u8], should_stop: &mut impl FnMut() -> bool) -> Option<Vec<CompactValue>> {
  let keys = ParseKeys::default();
  let mut events = Vec::new();
  let mut start = 0;
  for boundary in memchr_iter(b'\n', bytes) {
    if should_stop() {
      return None;
    }
    events.extend(parse_file_line(&bytes[start..boundary], &keys));
    start = boundary + 1;
  }
  if start < bytes.len() {
    if should_stop() {
      return None;
    }
    events.extend(parse_file_line(&bytes[start..], &keys));
  }
  if should_stop() {
    return None;
  }
  Some(events)
}

/// One event as an NDJSON line: its JSON with non-ASCII kept as is, and the newline.
pub(crate) fn encode_line(value: &Value) -> String {
  let mut line = text_repr::json_dumps(value, &APPEND_JSON);
  line.push('\n');
  line
}

/// Append one JSON line with non-ASCII kept as is, and sync its data to disk.
pub(crate) fn append(path: &Path, value: &Value) -> StoreResult<()> {
  let parent = path.parent().expect("an event-log path has a parent directory");
  fs::create_dir_all(parent).map_err(|error| StoreError::new(format!("{}: {error}", parent.display())))?;
  let mut file = OpenOptions::new()
    .create(true)
    .append(true)
    .open(path)
    .map_err(|error| StoreError::new(format!("{}: {error}", path.display())))?;
  let line = encode_line(value);
  file.write_all(line.as_bytes()).map_err(|error| StoreError::new(format!("{}: {error}", path.display())))?;
  file.sync_data().map_err(|error| StoreError::new(format!("{}: {error}", path.display())))?;
  Ok(())
}

/// Return the number of physical lines, charging one for a non-newline-terminated final line.
#[cfg(test)]
pub(crate) fn count_lines(path: &Path) -> StoreResult<usize> {
  let bytes = match fs::read(path) {
    Ok(bytes) => bytes,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
    Err(error) => return Err(StoreError::new(format!("{}: {error}", path.display()))),
  };
  if bytes.is_empty() {
    return Ok(0);
  }
  Ok(bytes.iter().filter(|byte| **byte == b'\n').count() + usize::from(!bytes.ends_with(b"\n")))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn skips_bad_lines_and_parses_a_partial_final_line() {
    let path = tempfile::NamedTempFile::new().unwrap();
    fs::write(path.path(), b"\nnot-json\n{\"id\":1}").unwrap();
    let events = read_file_compact(path.path()).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].to_value()["id"], 1);
    assert_eq!(count_lines(path.path()).unwrap(), 3);
  }

  #[test]
  fn append_keeps_unicode_and_default_spacing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.jsonl");
    append(&path, &serde_json::json!({"text": "你好", "id": "a"})).unwrap();
    assert_eq!(fs::read_to_string(path).unwrap(), "{\"text\": \"你好\", \"id\": \"a\"}\n");
  }

  /// The skip contract applies per line.
  #[test]
  fn parse_line_compact_applies_the_skip_contract_per_line() {
    let value_path = |line: &[u8]| parse_line_compact(line).map(|value| value.to_value());
    assert_eq!(value_path(br#"{"i": 1}"#), Some(serde_json::json!({"i": 1})));
    assert_eq!(value_path(b"  {\"i\": 1}  "), Some(serde_json::json!({"i": 1})));
    assert_eq!(value_path(b""), None);
    assert_eq!(value_path(b"   \n"), None);
    assert_eq!(value_path(b"{not json"), None);
    assert_eq!(value_path(br#"{"a": NaN}"#), None);
  }

  /// A line with a torn multibyte character parses with U+FFFD in its place.
  #[test]
  fn parse_line_compact_repairs_a_torn_multibyte_char_to_the_replacement_char() {
    assert_eq!(
      parse_line_compact(b"{\"text\": \"ok\xff\"}").map(|value| value.to_value()),
      Some(serde_json::json!({"text": "ok\u{fffd}"}))
    );
  }

  /// The reader parses each line once: a whole-file read of four lines attempts three parses, the
  /// blank line skipping, and no second pass touches the lines again.
  #[test]
  fn read_file_compact_parses_each_line_once() {
    let path = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
      path.path(),
      concat!(
        "{\"id\": 1, \"deep\": {\"a\": [1, 2.5, \"x\", null, true]}}\n",
        "\n",
        "{\"id\": 2, \"id\": 3}\n",
        "{not json\n",
      ),
    )
    .unwrap();
    LINE_PARSES.with(|count| count.set(0));
    let events = read_file_compact(path.path()).unwrap();
    assert_eq!(events.len(), 2, "blank and malformed lines skip, as the skip contract says");
    assert_eq!(events[1]["id"], CompactValue::PosInt(3), "a repeated key keeps its first position and last value");
    LINE_PARSES
      .with(|count| assert_eq!(count.get(), 3, "three parse attempts for four lines (the blank line skips), one pass"));
  }

  /// Every line the compact reader parses produces the same `CompactValue` the `Value` path builds:
  /// numbers keep their forms, key order stays, and the skip contract matches per line.
  #[test]
  fn compact_lines_equal_the_value_path_over_the_edge_corpus() {
    let lines: &[&[u8]] = &[
      br#"{"b": 1, "a": -2, "pi": 0.5, "s": "es\u00e9", "n": null, "flag": true, "list": [1, "y"]}"#,
      br#"{"dup": 1, "dup": 2}"#,
      br#"{"big": 18446744073709551615, "neg": -9223372036854775808}"#,
      br#""bare string""#,
      b"[1, 2, 3]",
      br#"{"deep": {"nested": {"x": [{}]}}}"#,
      b"   ",
      b"{broken",
      b"{\"text\": \"ok\xff\"}",
      "\u{65e5}\u{672c}\u{8a9e} raw bytes".as_bytes(),
      br#"{"a": 1} trailing"#,
    ];
    let file_keys = ParseKeys::default();
    for line in lines {
      let reference = serde_json::from_str::<serde_json::Value>(&String::from_utf8_lossy(line))
        .ok()
        .map(|value| CompactValue::from_value(&value));
      assert_eq!(parse_line_compact(line), reference, "line {line:?}");
      assert_eq!(parse_file_line(line, &file_keys), reference, "a whole-file parse of line {line:?}");
    }
  }

  /// A long line parses whole.
  #[test]
  fn read_file_compact_parses_a_long_line_whole() {
    let path = tempfile::NamedTempFile::new().unwrap();
    let payload = serde_json::json!({"blob": "x".repeat(32 * 1024)});
    fs::write(path.path(), format!("{payload}\n{{\"i\": 1}}")).unwrap();
    let events = read_file_compact(path.path()).unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].to_value(), payload);
    assert_eq!(events[1]["i"], CompactValue::PosInt(1));
  }

  /// A split read gives the events a single-thread read gives, in the file's order: a split never cuts a line,
  /// including a line longer than a run across a split point, blank and malformed lines, and a final line without `\n`.
  #[test]
  fn a_split_read_gives_the_events_of_a_whole_read() {
    let mut text = String::new();
    for index in 0..4_000 {
      text.push_str(&format!("{{\"id\": {index}, \"pad\": \"{}\"}}\n", "p".repeat(index % 600)));
      if index % 397 == 0 {
        text.push_str("\n{broken\n");
      }
      if index == 2_000 {
        text.push_str(&format!("{}\n", serde_json::json!({"blob": "x".repeat(600 << 10)})));
      }
    }
    text.push_str("{\"id\": \"last\"}");
    let sizes = SplitSizes { min_bytes: 1 << 20, chunk_bytes: 128 << 10, max_threads: 8 };
    assert!(text.len() >= sizes.min_bytes && text.len() >= sizes.chunk_bytes * sizes.max_threads, "the text splits");
    let split = split_lines_of_sized(text.as_bytes(), &parse_file_line, &sizes);
    assert_eq!(split.len(), 4_002, "every object line, the blob and the final line");
    assert_eq!(split, lines_of(text.as_bytes(), &parse_file_line));
  }

  #[test]
  fn cancelled_read_checks_each_line_and_returns_no_partial_values() {
    let path = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(path.path(), b"{\"id\":1}\n{\"id\":2}\n{\"id\":3}\n").unwrap();
    LINE_PARSES.with(|count| count.set(0));
    let mut checks = 0;
    let events = read_file_compact_until(path.path(), &mut || {
      checks += 1;
      checks == 4
    })
    .unwrap();

    assert_eq!(checks, 4, "the read checked once before opening and before each parsed line");
    assert!(events.is_none(), "cancellation discards the parsed prefix");
    LINE_PARSES.with(|count| assert_eq!(count.get(), 2, "the first two lines were parsed before cancellation"));
  }

  #[test]
  fn cancellation_after_the_last_line_discards_the_fully_parsed_file() {
    let path = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(path.path(), b"{\"id\":1}\n{\"id\":2}\n{\"id\":3}\n").unwrap();
    LINE_PARSES.with(|count| count.set(0));
    let mut checks = 0;
    let events = read_file_compact_until(path.path(), &mut || {
      checks += 1;
      checks == 5
    })
    .unwrap();

    assert_eq!(checks, 5, "the read checks for cancellation after the final line");
    assert!(events.is_none(), "a stop after parsing the final line discards the event cache");
    LINE_PARSES.with(|count| assert_eq!(count.get(), 3, "all three lines were parsed before the final check"));
  }
}
