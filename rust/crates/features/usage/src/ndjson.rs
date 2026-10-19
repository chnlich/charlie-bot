use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::Context;
use serde_json::Value;

pub fn parse_marker_lines(path: &Path, markers: &[&[u8]]) -> anyhow::Result<Vec<Value>> {
  let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
  let mut reader = BufReader::new(file);
  let mut line = Vec::new();
  let mut objects = Vec::new();
  loop {
    line.clear();
    if reader.read_until(b'\n', &mut line).with_context(|| format!("reading {}", path.display()))? == 0 {
      break;
    }
    if line.last() != Some(&b'\n') || !markers.iter().any(|marker| contains(&line, marker)) {
      continue;
    }
    let parsed = serde_json::from_slice(&line).or_else(|_| serde_json::from_str(&String::from_utf8_lossy(&line)));
    if let Ok(value) = parsed {
      objects.push(value);
    }
  }
  Ok(objects)
}

fn contains(line: &[u8], marker: &[u8]) -> bool {
  !marker.is_empty() && line.windows(marker.len()).any(|window| window == marker)
}

#[cfg(test)]
mod tests {
  use std::fs;

  use super::*;

  #[test]
  fn marker_parser_keeps_complete_valid_lines_and_drops_bad_lines_and_tail() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.ndjson");
    fs::write(
      &path,
      b"{\"type\": \"result\", \"value\": 1}\n{\"type\": \"result\", broken}\n{\"other\": true}\n{\"type\": \"result\", \"value\": 2}",
    )
    .unwrap();
    let values = parse_marker_lines(&path, &[b"\"type\": \"result\""]).unwrap();
    assert_eq!(values, [serde_json::json!({"type": "result", "value": 1})]);
  }

  #[test]
  fn marker_parser_accepts_a_valid_line_after_invalid_utf8_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.ndjson");
    fs::write(&path, b"{\"type\": \"result\", \"model\": \"bad\xff\"}\n").unwrap();
    let values = parse_marker_lines(&path, &[b"\"type\": \"result\""]).unwrap();
    assert_eq!(values[0]["model"], "bad�");
  }
}
