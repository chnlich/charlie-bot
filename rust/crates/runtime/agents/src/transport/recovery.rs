//! Whole-file raw-log readers used after a producer exits.

use std::fs;
use std::path::Path;

use anyhow::Context;
use serde_json::Value;
use serde_json::value::RawValue;
use time::OffsetDateTime;

/// A filter whose rejected type is provable from the raw line's first JSON key.
#[derive(Clone, Debug)]
pub struct TypeLineFilter {
  keep_types: Vec<String>,
}

impl TypeLineFilter {
  /// Keep the named event types while parsing every line the prefix cannot classify.
  pub fn new(types: impl IntoIterator<Item = impl Into<String>>) -> Self {
    Self { keep_types: types.into_iter().map(Into::into).collect() }
  }

  fn rejects(&self, line: &[u8]) -> bool {
    let Some(mut rest) = line.strip_prefix(br#"{"type""#) else {
      return false;
    };
    rest = trim_horizontal_space(rest);
    let Some(after_colon) = rest.strip_prefix(b":") else {
      return false;
    };
    let rest = trim_horizontal_space(after_colon);
    if !rest.starts_with(b"\"") {
      return false;
    }
    // `RawValue` finds the end of the quoted token without decoding it, as Python's first-quote scan does.
    let text = String::from_utf8_lossy(rest);
    let Some(Ok(token)) = serde_json::Deserializer::from_str(&text).into_iter::<Box<RawValue>>().next() else {
      return false;
    };
    let Some(event_type) = token.get().strip_prefix('"').and_then(|value| value.strip_suffix('"')) else {
      return false;
    };
    !self.keep_types.iter().any(|kind| kind == event_type)
  }
}

fn trim_horizontal_space(mut bytes: &[u8]) -> &[u8] {
  while matches!(bytes.first(), Some(b' ' | b'\t')) {
    bytes = &bytes[1..];
  }
  bytes
}

/// Parse raw NDJSON lines, skipping blank and malformed lines.
pub fn parse_raw_lines(raw_bytes: &[u8]) -> Vec<Value> {
  raw_bytes.split(|byte| *byte == b'\n').filter_map(parse_line).collect()
}

/// Parse the raw-log prefix ending at a cursor byte offset.
pub fn raw_events_before_cursor(raw_path: &Path, cursor: u64) -> anyhow::Result<Vec<Value>> {
  if cursor == 0 {
    return Ok(Vec::new());
  }
  let raw_bytes = fs::read(raw_path)?;
  let cursor = usize::try_from(cursor).context("raw-log cursor does not fit in memory")?;
  anyhow::ensure!(cursor <= raw_bytes.len(), "raw-log cursor is beyond the end of {}", raw_path.display());
  anyhow::ensure!(
    cursor == 0 || raw_bytes[cursor - 1] == b'\n',
    "raw-log cursor does not end at a line boundary in {}",
    raw_path.display()
  );
  Ok(parse_raw_lines(&raw_bytes[..cursor]))
}

fn parse_line(line: &[u8]) -> Option<Value> {
  if line.is_empty() || line.iter().all(u8::is_ascii_whitespace) {
    return None;
  }
  match serde_json::from_slice(line) {
    Ok(value) => Some(value),
    Err(error) => match std::str::from_utf8(line) {
      Ok(_) => {
        tracing::debug!(target: "charliebot_agents", error = %error, "raw_line_not_json");
        None
      }
      Err(_) => match serde_json::from_str(&String::from_utf8_lossy(line)) {
        Ok(value) => Some(value),
        Err(repaired_error) => {
          tracing::debug!(target: "charliebot_agents", error = %repaired_error, "raw_line_not_json");
          None
        }
      },
    },
  }
}

/// Apply a stateful raw-event translator to each accepted event in file order.
pub fn project_raw_events<T>(events: impl IntoIterator<Item = Value>, mut translate: T) -> anyhow::Result<Vec<Value>>
where
  T: FnMut(Value) -> anyhow::Result<Vec<Value>>,
{
  let mut projected = Vec::new();
  for event in events {
    projected.extend(translate(event)?);
  }
  Ok(projected)
}

/// Read, parse, and project the raw file; a line filter skips only provable type mismatches.
pub fn project_raw_file<T>(
  raw_path: &Path,
  mut translate: T,
  line_filter: Option<&TypeLineFilter>,
) -> anyhow::Result<Vec<Value>>
where
  T: FnMut(Value) -> anyhow::Result<Vec<Value>>,
{
  let raw_bytes = fs::read(raw_path)?;
  let events = raw_bytes
    .split(|byte| *byte == b'\n')
    .filter(|line| !line_filter.is_some_and(|filter| filter.rejects(line)))
    .filter_map(parse_line);
  project_raw_events(events, &mut translate)
}

/// Return the raw log's final modification time in UTC.
pub fn raw_completion_time(raw_path: &Path) -> Option<OffsetDateTime> {
  let completion_path = super::final_stdout::completion_path(raw_path.parent()?);
  let raw_path = if raw_path.file_name()? == charliebot_store::run_records::RAW_LOG_NAME {
    completion_path.as_path()
  } else {
    raw_path
  };
  let modified = fs::metadata(raw_path).ok()?.modified().ok()?;
  Some(OffsetDateTime::from(modified))
}

/// Return zero for a successful last result and minus one when it is absent or failed.
pub fn result_exit_code(events: &[Value]) -> i32 {
  let result = events.iter().rev().find(|event| event.get("type").and_then(Value::as_str) == Some("result"));
  let Some(result) = result else {
    return -1;
  };
  let subtype_success = result.get("subtype").is_none_or(|value| value.is_null() || value == "success");
  let error_absent = result
    .get("is_error")
    .is_none_or(|value| value.is_null() || value == false || value.as_f64().is_some_and(|number| number == 0.0));
  if subtype_success && error_absent { 0 } else { -1 }
}

/// Scan a raw log that may be missing and return its events, last result, and Python-compatible exit code.
pub fn scan_result_exit<T>(raw_path: &Path, translate: T) -> anyhow::Result<(Vec<Value>, Option<Value>, i32)>
where
  T: FnMut(Value) -> anyhow::Result<Vec<Value>>,
{
  let events = if raw_path.is_file() { project_raw_file(raw_path, translate, None)? } else { Vec::new() };
  let result = events.iter().rev().find(|event| event.get("type").and_then(Value::as_str) == Some("result")).cloned();
  let exit_code = match super::final_stdout::read_outcome(raw_path.parent().expect("raw log has a parent"))? {
    Some(outcome) if outcome.guard_error.is_none() => outcome.exit_code,
    Some(_) => -1,
    None => result_exit_code(&events),
  };
  Ok((events, result, exit_code))
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn raw_projection_filters_by_provable_first_type_key() {
    let root = tempfile::tempdir().unwrap();
    let raw = root.path().join("agent.raw.ndjson");
    fs::write(
      &raw,
      b"{\"type\": \"user\"}\n{\"type\": \"assistant\", \"seq\": 1}\n{\"duration_api_ms\": 1, \"type\": \"result\"}\n",
    )
    .unwrap();
    let filter = TypeLineFilter::new(["assistant"]);
    let events = project_raw_file(&raw, |event| Ok(vec![event]), Some(&filter)).unwrap();
    assert_eq!(events, [json!({"type":"assistant", "seq":1}), json!({"duration_api_ms":1,"type":"result"})]);
  }

  #[test]
  fn exit_code_uses_the_last_result_event() {
    assert_eq!(result_exit_code(&[json!({"type":"result", "subtype":"success"})]), 0);
    assert_eq!(result_exit_code(&[json!({"type":"result", "subtype":null, "is_error":null})]), 0);
    assert_eq!(
      result_exit_code(&[json!({"type":"result", "subtype":"success"}), json!({"type":"result", "subtype":"error"})]),
      -1
    );
    assert_eq!(result_exit_code(&[]), -1);
  }

  #[test]
  fn recovery_scan_returns_last_result_and_handles_a_missing_log() {
    let root = tempfile::tempdir().unwrap();
    let raw = root.path().join("agent.raw.ndjson");
    fs::write(&raw, b"{\"type\":\"result\",\"subtype\":\"success\"}\n{\"type\":\"result\",\"subtype\":\"error\"}\n")
      .unwrap();
    let (events, result, exit_code) = scan_result_exit(&raw, |event| Ok(vec![event])).unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(result.unwrap()["subtype"], "error");
    assert_eq!(exit_code, -1);
    let (events, result, exit_code) = scan_result_exit(&root.path().join("missing"), |event| Ok(vec![event])).unwrap();
    assert!(events.is_empty());
    assert!(result.is_none());
    assert_eq!(exit_code, -1);
  }

  #[test]
  fn raw_projection_returns_translator_errors() {
    let root = tempfile::tempdir().unwrap();
    let raw = root.path().join("agent.raw.ndjson");
    fs::write(&raw, b"{\"type\":\"assistant\"}\n").unwrap();
    let error = project_raw_file(&raw, |_| Err(anyhow::anyhow!("synthetic translator failure")), None).unwrap_err();
    assert_eq!(error.to_string(), "synthetic translator failure");
  }

  #[test]
  fn completion_time_comes_from_raw_file_mtime() {
    let root = tempfile::tempdir().unwrap();
    let raw = root.path().join("agent.raw.ndjson");
    fs::write(&raw, b"{}").unwrap();
    assert!(raw_completion_time(&raw).is_some());
    assert_eq!(raw_completion_time(&root.path().join("missing")), None);
  }
}
