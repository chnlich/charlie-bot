use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use charliebot_core::pyfmt::{self, JsonStyle};
use serde_json::Value;

use crate::compact::CompactValue;
use crate::{StoreError, StoreResult};

const APPEND_JSON: JsonStyle =
  JsonStyle { ensure_ascii: false, sort_keys: false, item_separator: ", ", key_separator: ": ", indent: None };

/// Parse one NDJSON line. Blank and malformed lines are skipped. Invalid UTF-8 is replaced with U+FFFD before
/// the parse, as `src/infra/ndjson.py::parse_ndjson_line` repairs it, so such a line can still parse. A final
/// partial line is parsed just like any other line.
///
/// One pass: the line deserializes straight into [`CompactValue`] — the reader's only event form —
/// through the visitor in `compact`, which produces the same value a `serde_json::Value` round trip
/// would have.
pub(crate) fn parse_line_compact(raw: &[u8]) -> Option<CompactValue> {
  if raw.iter().all(u8::is_ascii_whitespace) {
    return None;
  }
  #[cfg(test)]
  LINE_PARSES.with(|count| count.set(count.get() + 1));
  let text = String::from_utf8_lossy(raw);
  serde_json::from_str(&text).ok()
}

// How many lines this thread's parser attempted, a test-only count that proves a reader parses each
// line once. Thread-local because tests run in parallel and the read runs on its caller's thread.
#[cfg(test)]
thread_local! {
  static LINE_PARSES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Read every physical line of an NDJSON file, including a final line without `\n`.
pub(crate) fn read_lines(path: &Path) -> StoreResult<Vec<Vec<u8>>> {
  let file = match fs::File::open(path) {
    Ok(file) => file,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
    Err(error) => return Err(StoreError::new(format!("{}: {error}", path.display()))),
  };
  BufReader::new(file)
    .split(b'\n')
    .collect::<Result<Vec<_>, _>>()
    .map_err(|error| StoreError::new(format!("{}: {error}", path.display())))
}

/// Read every physical line unless `should_stop` requests cancellation between lines.
pub(crate) fn read_lines_until(
  path: &Path,
  should_stop: &mut impl FnMut() -> bool,
) -> StoreResult<Option<Vec<Vec<u8>>>> {
  if should_stop() {
    return Ok(None);
  }
  let file = match fs::File::open(path) {
    Ok(file) => file,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Some(Vec::new())),
    Err(error) => return Err(StoreError::new(format!("{}: {error}", path.display()))),
  };
  let mut lines = Vec::new();
  for line in BufReader::new(file).split(b'\n') {
    if should_stop() {
      return Ok(None);
    }
    lines.push(line.map_err(|error| StoreError::new(format!("{}: {error}", path.display())))?);
  }
  if should_stop() {
    return Ok(None);
  }
  Ok(Some(lines))
}

/// Parse every physical line unless `should_stop` requests cancellation between lines.
///
/// A cancelled read returns no values so callers cannot publish a partial event cache.
/// Parse every physical line into the compact event form, including a final line without `\n`.
pub(crate) fn read_file_compact(path: &Path) -> StoreResult<Vec<CompactValue>> {
  Ok(read_lines(path)?.iter().filter_map(|raw| parse_line_compact(raw)).collect())
}

/// Parse every physical line into the compact event form unless `should_stop` requests
/// cancellation between lines. A cancelled read returns no events.
pub(crate) fn read_file_compact_until(
  path: &Path,
  should_stop: &mut impl FnMut() -> bool,
) -> StoreResult<Option<Vec<CompactValue>>> {
  Ok(read_lines_until(path, should_stop)?.map(|lines| lines.iter().filter_map(|raw| parse_line_compact(raw)).collect()))
}

/// One event as an NDJSON line: Python `json.dumps(..., ensure_ascii=False)` and the newline.
pub(crate) fn encode_line(value: &Value) -> String {
  let mut line = pyfmt::json_dumps(value, &APPEND_JSON);
  line.push('\n');
  line
}

/// Append one Python `json.dumps(..., ensure_ascii=False)` line and sync its data to disk.
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
  fn append_keeps_unicode_and_python_default_spacing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.jsonl");
    append(&path, &serde_json::json!({"text": "你好", "id": "a"})).unwrap();
    assert_eq!(fs::read_to_string(path).unwrap(), "{\"text\": \"你好\", \"id\": \"a\"}\n");
  }

  /// `core/test_ndjson.py::test_parse_ndjson_line_applies_the_skip_contract_per_line`.
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

  /// `core/test_ndjson.py::test_parse_ndjson_line_bytes_torn_multibyte_parses_as_replacement_char`.
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
    ];
    for line in lines {
      let reference = serde_json::from_str::<serde_json::Value>(&String::from_utf8_lossy(line))
        .ok()
        .map(|value| CompactValue::from_value(&value));
      assert_eq!(parse_line_compact(line), reference, "line {line:?}");
    }
  }

  /// `core/test_ndjson.py::test_parse_ndjson_file_multi_megabyte_lines_parse_whole`.
  #[test]
  fn read_file_compact_parses_a_multi_megabyte_line_whole() {
    let path = tempfile::NamedTempFile::new().unwrap();
    let payload = serde_json::json!({"blob": "x".repeat(8 * 1024 * 1024)});
    fs::write(path.path(), format!("{payload}\n{{\"i\": 1}}")).unwrap();
    let events = read_file_compact(path.path()).unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].to_value(), payload);
    assert_eq!(events[1]["i"], CompactValue::PosInt(1));
  }

  #[test]
  fn cancelled_read_checks_each_line_and_returns_no_partial_values() {
    let path = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(path.path(), b"{\"id\":1}\n{\"id\":2}\n{\"id\":3}\n").unwrap();
    let mut checks = 0;
    let events = read_file_compact_until(path.path(), &mut || {
      checks += 1;
      checks == 4
    })
    .unwrap();

    assert_eq!(checks, 4, "the read checked once before opening and before each parsed line");
    assert!(events.is_none(), "cancellation discards the parsed prefix");
  }
}
