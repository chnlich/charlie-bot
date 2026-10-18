//! Codex rollout records and the readers they share (`src/backends/codex/codex_usage.py`). Codex writes one JSON line
//! per record into `<codex home>/sessions/**/rollout-*.jsonl`.

use std::fs::{self, Metadata};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde_json::{Map, Value, json};

use super::text::{EMPTY_OBJECT, truthy_object};

/// The rollout record-type names: `session_meta` opens a file, `turn_context` carries the model in force, and
/// `event_msg` wraps the `token_count` payload the readers read.
pub const SESSION_META: &str = "session_meta";
pub const TURN_CONTEXT: &str = "turn_context";
pub const EVENT_MSG: &str = "event_msg";
pub const TOKEN_COUNT: &str = "token_count";

/// The latest usage of one rollout: the context reading and the cumulative totals that price the session.
#[derive(Clone, Debug)]
pub struct RolloutUsage {
  pub context_tokens: Value,
  pub context_full: Value,
  pub total_token_usage: Option<Map<String, Value>>,
  pub model: String,
}

/// The `token_count` payload an `event_msg` record wraps, or `None` for any other record. A payload that is not an
/// object is not read as a token count.
pub fn token_count_payload(event: &Map<String, Value>) -> Option<&Map<String, Value>> {
  if text_of(event, "type") != Some(EVENT_MSG) {
    return None;
  }
  let payload = truthy_object(event, "payload")?;
  (text_of(payload, "type") == Some(TOKEN_COUNT)).then_some(payload)
}

/// A rollout line's context tokens, context window and cumulative totals.
type UsageParts = (Value, Value, Option<Map<String, Value>>);

/// The newest usage in one rollout, read backwards from its last line. `None` when no line carries a token count.
pub fn extract_latest_rollout_usage(path: &Path) -> anyhow::Result<Option<RolloutUsage>> {
  let bytes = fs::read(path).with_context(|| format!("reading rollout {}", path.display()))?;
  let mut usage: Option<UsageParts> = None;
  let mut model: Option<String> = None;
  for line in bytes.rsplit(|byte| *byte == b'\n') {
    let Some(event) = parse_event_line(line, path)? else {
      continue;
    };
    if usage.is_none() {
      usage = usage_of_event(&event);
    }
    if model.is_none() {
      model = model_of_event(&event);
    }
    if usage.is_some() && model.is_some() {
      break;
    }
  }
  Ok(usage.map(|(context_tokens, context_full, total_token_usage)| RolloutUsage {
    context_tokens,
    context_full,
    total_token_usage,
    model: model.unwrap_or_default(),
  }))
}

/// The context tokens, the context window and the totals of a `token_count` event, or `None` when a count is missing.
fn usage_of_event(event: &Map<String, Value>) -> Option<UsageParts> {
  let payload = token_count_payload(event)?;
  let info = truthy_object(payload, "info").unwrap_or(&*EMPTY_OBJECT);
  let last = truthy_object(info, "last_token_usage").unwrap_or(&*EMPTY_OBJECT);
  // Codex reports the active prompt window in last_token_usage.input_tokens. total_token_usage is cumulative for the
  // session, and cached_input_tokens is a subset of the input, not an extra component of the window.
  let input_tokens = last.get("input_tokens").filter(|value| !value.is_null())?;
  let context_full = info.get("model_context_window").filter(|value| !value.is_null())?;
  let total = info.get("total_token_usage").and_then(Value::as_object).map(|total| {
    let mut totals = Map::new();
    for key in ["input_tokens", "cached_input_tokens", "output_tokens"] {
      totals.insert(key.to_string(), total.get(key).cloned().unwrap_or_else(|| json!(0)));
    }
    totals
  });
  Some((input_tokens.clone(), context_full.clone(), total))
}

/// The model a `session_meta` or `turn_context` record names, when it names a non-empty one.
fn model_of_event(event: &Map<String, Value>) -> Option<String> {
  if text_of(event, "type") != Some(TURN_CONTEXT) {
    return None;
  }
  let payload = truthy_object(event, "payload").unwrap_or(&*EMPTY_OBJECT);
  payload.get("model").and_then(Value::as_str).filter(|model| !model.is_empty()).map(str::to_string)
}

/// One line parsed as a record. A blank line and a line that is not JSON read as `None`; the latter is logged, as the
/// Python reader does. A line that is JSON but not an object is an error, as Python's attribute read on it is.
pub fn parse_event_line(line: &[u8], path: &Path) -> anyhow::Result<Option<Map<String, Value>>> {
  if line.iter().all(u8::is_ascii_whitespace) {
    return Ok(None);
  }
  match serde_json::from_slice::<Value>(line) {
    Ok(Value::Object(event)) => Ok(Some(event)),
    Ok(other) => bail!("{}: rollout line is not a JSON object: {other}", path.display()),
    Err(error) => {
      tracing::debug!(target: "charliebot_agents", path = %path.display(), error = %error, "codex_rollout_parse_skip");
      Ok(None)
    }
  }
}

/// The records of every file under `root` whose name `keep` accepts, in the order the Python walk yields them: a
/// directory's own files before its subdirectories'. A symlinked directory is not entered.
pub fn walk_files(root: &Path, keep: impl Fn(&str) -> bool) -> io::Result<Vec<PathBuf>> {
  let mut found = Vec::new();
  let mut stack = vec![root.to_path_buf()];
  while let Some(dir) = stack.pop() {
    for entry in fs::read_dir(&dir)? {
      let entry = entry?;
      let path = entry.path();
      if entry.file_type()?.is_dir() {
        stack.push(path);
      } else if keep(&entry.file_name().to_string_lossy()) {
        found.push(path);
      }
    }
  }
  Ok(found)
}

/// The file's modification time in nanoseconds (`st_mtime_ns`).
pub fn mtime_ns(metadata: &Metadata) -> i128 {
  i128::from(metadata.mtime()) * 1_000_000_000 + i128::from(metadata.mtime_nsec())
}

/// The file's modification time in seconds with its fraction (`st_mtime`).
pub fn mtime_secs(metadata: &Metadata) -> f64 {
  metadata.mtime() as f64 + metadata.mtime_nsec() as f64 / 1e9
}

pub fn text_of<'a>(map: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
  map.get(key).and_then(Value::as_str)
}

/// The name a rollout file is known by: `rollout-` and `.jsonl` removed.
pub fn rollout_stem(name: &str) -> Option<&str> {
  name.strip_prefix("rollout-").and_then(|rest| rest.strip_suffix(".jsonl"))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn latest_usage_reads_the_last_count_and_the_model_in_force() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("rollout-2026-10-18T10-00-00-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.jsonl");
    let lines = [
      json!({"type": "turn_context", "payload": {"model": "gpt-5.5"}}),
      json!({"type": "event_msg", "payload": {"type": "token_count", "info": {
        "last_token_usage": {"input_tokens": 10, "cached_input_tokens": 0, "output_tokens": 1},
        "model_context_window": 100, "total_token_usage": {"input_tokens": 10}}}}),
      json!({"type": "event_msg", "payload": {"type": "token_count", "info": {
        "last_token_usage": {"input_tokens": 500, "cached_input_tokens": 400, "output_tokens": 20},
        "model_context_window": 1000, "total_token_usage": {"input_tokens": 510, "cached_input_tokens": 400,
        "output_tokens": 21}}}}),
    ];
    let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
    fs::write(&path, text).unwrap();
    let usage = extract_latest_rollout_usage(&path).unwrap().unwrap();
    assert_eq!(usage.context_tokens, json!(500));
    assert_eq!(usage.context_full, json!(1000));
    assert_eq!(usage.model, "gpt-5.5");
    assert_eq!(usage.total_token_usage.unwrap()["cached_input_tokens"], json!(400));
  }
}
