use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use charliebot_core::pyfmt::{self, JsonStyle};
use serde_json::Value;

use crate::{StoreError, StoreResult};

const APPEND_JSON: JsonStyle =
  JsonStyle { ensure_ascii: false, sort_keys: false, item_separator: ", ", key_separator: ": ", indent: None };

/// Parse one NDJSON line. Blank and malformed lines are skipped. Invalid UTF-8 is replaced with U+FFFD before
/// the parse, as `src/infra/ndjson.py::parse_ndjson_line` repairs it, so such a line can still parse. A final
/// partial line is parsed just like any other line.
pub(crate) fn parse_line(raw: &[u8]) -> Option<Value> {
  if raw.iter().all(u8::is_ascii_whitespace) {
    return None;
  }
  let text = String::from_utf8_lossy(raw);
  serde_json::from_str(&text).ok()
}

/// Parse every physical line in an NDJSON file, including a final line without `\n`.
pub(crate) fn read_file(path: &Path) -> StoreResult<Vec<Value>> {
  let bytes = match fs::read(path) {
    Ok(bytes) => bytes,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
    Err(error) => return Err(StoreError::new(format!("{}: {error}", path.display()))),
  };
  Ok(bytes.split(|byte| *byte == b'\n').filter_map(parse_line).collect())
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
  let mut line = pyfmt::json_dumps(value, &APPEND_JSON);
  line.push('\n');
  file.write_all(line.as_bytes()).map_err(|error| StoreError::new(format!("{}: {error}", path.display())))?;
  file.sync_data().map_err(|error| StoreError::new(format!("{}: {error}", path.display())))?;
  Ok(())
}

/// Return the number of physical lines, charging one for a non-newline-terminated final line.
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
    assert_eq!(read_file(path.path()).unwrap(), vec![serde_json::json!({"id": 1})]);
    assert_eq!(count_lines(path.path()).unwrap(), 3);
  }

  #[test]
  fn append_keeps_unicode_and_python_default_spacing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.jsonl");
    append(&path, &serde_json::json!({"text": "你好", "id": "a"})).unwrap();
    assert_eq!(fs::read_to_string(path).unwrap(), "{\"text\": \"你好\", \"id\": \"a\"}\n");
  }
}
