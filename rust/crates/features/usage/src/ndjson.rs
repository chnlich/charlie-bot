use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::Context;
use memchr::memmem;
use serde_json::Value;

pub fn parse_marker_lines(path: &Path, markers: &[&[u8]]) -> anyhow::Result<Vec<Value>> {
  let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
  let mut reader = BufReader::new(file);
  let mut line = Vec::new();
  let mut objects = Vec::new();
  let finders = markers.iter().filter(|marker| !marker.is_empty()).map(memmem::Finder::new).collect::<Vec<_>>();
  loop {
    line.clear();
    if reader.read_until(b'\n', &mut line).with_context(|| format!("reading {}", path.display()))? == 0 {
      break;
    }
    if line.last() != Some(&b'\n') || !finders.iter().any(|finder| finder.find(&line).is_some()) {
      continue;
    }
    let parsed = serde_json::from_slice(&line).or_else(|_| serde_json::from_str(&String::from_utf8_lossy(&line)));
    if let Ok(value) = parsed {
      objects.push(value);
    }
  }
  Ok(objects)
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

  #[test]
  fn parse_marker_lines_across_chunks_around_a_giant_line() {
    // A no-marker line spanning many reads parses never: the marker lines around it come out in file order, and the
    // trailing fragment without its newline waits.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("giant.ndjson");
    let lines = [
      "{\"type\": \"context\", \"model\": \"clc-x\"}".to_string(),
      format!("{{\"pad\": \"{}\"}}", "q".repeat(500)),
      "{\"type\": \"result\", \"usage\": {\"input_tokens\": 3, \"output_tokens\": 4}}".to_string(),
      format!("{{\"pad2\": \"{}\"}}", "r".repeat(130)),
      "{\"type\": \"result\", \"usage\": {\"input_tokens\": 5, \"output_tokens\": 6}}".to_string(),
    ];
    fs::write(&path, format!("{}\n{{\"trailing\": \"fragment\"}}", lines.join("\n"))).unwrap();
    let objects = parse_marker_lines(&path, &[b"\"type\": \"context\"", b"\"type\": \"result\""]).unwrap();
    let shape = objects.iter().map(|object| (object["type"].as_str(), object["usage"]["input_tokens"].as_i64()));
    assert_eq!(
      shape.collect::<Vec<_>>(),
      [(Some("context"), None), (Some("result"), Some(3)), (Some("result"), Some(5))]
    );
  }
}
