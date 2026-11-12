//! The startup-only state files: a JSON header line naming the format version, then one tab-separated row per record.
//!
//! The task walk's skip proof and the trigger walk's index both hold the rows a boot may serve without opening a file.
//! Each crate reads the rows back into its own record type; the file format has this one writer and this one parser.

use std::io;
use std::path::Path;

use serde_json::{Value, json};

use crate::fsutil;
use crate::text_repr::{self, DEFAULT_JSON};

/// The separator between the fields of one row. A field holds no tab.
pub const FIELD_SEPARATOR: char = '\t';

/// Write `rows` to `path`: a header line naming `version`, then one line per row with its fields joined by
/// [`FIELD_SEPARATOR`]. The parent directory is created, and the file is published atomically, so a reader sees the
/// previous file or the new one.
pub fn write(path: &Path, version: u64, rows: &[Vec<String>]) -> io::Result<()> {
  let mut body = String::with_capacity(rows.len().saturating_mul(96) + 64);
  body.push_str(&text_repr::json_dumps(&json!({"version": version}), &DEFAULT_JSON));
  body.push('\n');
  for row in rows {
    for (index, field) in row.iter().enumerate() {
      if index > 0 {
        body.push(FIELD_SEPARATOR);
      }
      body.push_str(field);
    }
    body.push('\n');
  }
  if let Some(parent) = path.parent() {
    std::fs::create_dir_all(parent)?;
  }
  fsutil::atomic_write_text(path, &body)
}

/// The fields of each row `raw` holds, in file order, or `None` when its first line is not a header of `version`. An
/// empty line holds no row and is dropped.
pub fn parse(raw: &[u8], version: u64) -> Option<Vec<Vec<String>>> {
  let text = std::str::from_utf8(raw).ok()?;
  let mut lines = text.split('\n');
  let header: Value = serde_json::from_str(lines.next()?).ok()?;
  if header.get("version")?.as_u64()? != version {
    return None;
  }
  Some(
    lines
      .filter(|line| !line.is_empty())
      .map(|line| line.split(FIELD_SEPARATOR).map(str::to_string).collect())
      .collect(),
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_written_file_parses_back_into_its_rows() {
    let dir = tempfile::tempdir().expect("temporary directory exists");
    let path = dir.path().join("state").join("rows");
    let rows = vec![vec!["session-a".to_string(), "12".to_string()], vec!["session-b".to_string(), String::new()]];

    write(&path, 3, &rows).expect("the file writes");

    assert_eq!(parse(&std::fs::read(&path).expect("the file reads"), 3), Some(rows));
  }

  #[test]
  fn a_foreign_version_or_a_non_json_header_serves_no_row() {
    let dir = tempfile::tempdir().expect("temporary directory exists");
    let path = dir.path().join("rows");
    write(&path, 3, &[vec!["session-a".to_string(), "12".to_string()]]).expect("the file writes");

    let raw = std::fs::read(&path).expect("the file reads");
    assert_eq!(parse(&raw, 4), None, "a header of another version serves no row");
    assert_eq!(parse(b"not a header\nsession-a\t12\n", 3), None, "a first line that is not JSON serves no row");
  }
}
