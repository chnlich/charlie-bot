//! Codex rollout records and the readers they share (`src/backends/codex/codex_usage.py`). Codex writes one JSON line
//! per record into `<codex home>/sessions/**/rollout-*.jsonl`.

use std::fs::{self, Metadata};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use memchr::memrchr;
use serde_json::{Map, Value, json};

use super::text::object_or_empty;

/// The rollout record-type names: `session_meta` opens a file, `turn_context` carries the model in force, and
/// `event_msg` wraps the `token_count` payload the readers read.
pub const SESSION_META: &str = "session_meta";
pub const TURN_CONTEXT: &str = "turn_context";
pub const EVENT_MSG: &str = "event_msg";
pub const TOKEN_COUNT: &str = "token_count";
const INITIAL_ROLLOUT_TAIL_BYTES: usize = 4096;

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
pub fn token_count_payload(event: &Map<String, Value>) -> anyhow::Result<Option<&Map<String, Value>>> {
  if text_of(event, "type") != Some(EVENT_MSG) {
    return Ok(None);
  }
  let payload = object_or_empty(event, "payload")?;
  Ok((text_of(payload, "type") == Some(TOKEN_COUNT)).then_some(payload))
}

/// A rollout line's context tokens, context window and cumulative totals.
type UsageParts = (Value, Value, Option<Map<String, Value>>);

/// The newest usage in one rollout, read backwards from its last line. `None` when no line carries a token count.
pub fn extract_latest_rollout_usage(path: &Path) -> anyhow::Result<Option<RolloutUsage>> {
  let mut file = fs::File::open(path).with_context(|| format!("opening rollout {}", path.display()))?;
  extract_latest_rollout_usage_from(&mut file, path)
}

/// Scan backward in growing byte windows, as `src/infra/ndjson.py:404-468` does with an mmap and `rfind`.
fn extract_latest_rollout_usage_from<R: Read + Seek>(
  file: &mut R,
  path: &Path,
) -> anyhow::Result<Option<RolloutUsage>> {
  let mut start = file.seek(SeekFrom::End(0)).with_context(|| format!("seeking rollout {}", path.display()))?;
  let mut window = Vec::new();
  let mut window_bytes = INITIAL_ROLLOUT_TAIL_BYTES;
  let mut usage: Option<UsageParts> = None;
  let mut model: Option<String> = None;
  loop {
    if start > 0 {
      let next_start = start.saturating_sub(window_bytes as u64);
      file.seek(SeekFrom::Start(next_start)).with_context(|| format!("seeking rollout {}", path.display()))?;
      let mut preceding = vec![0; usize::try_from(start - next_start).expect("tail window fits usize")];
      file.read_exact(&mut preceding).with_context(|| format!("reading rollout {}", path.display()))?;
      preceding.extend_from_slice(&window);
      window = preceding;
      start = next_start;
    }

    let mut end = window.len();
    while end > 0 {
      let Some(newline) = memrchr(b'\n', &window[..end]) else {
        if start == 0 {
          inspect_rollout_line(&window[..end], path, &mut usage, &mut model)?;
        }
        break;
      };
      if newline + 1 < end {
        inspect_rollout_line(&window[newline + 1..end], path, &mut usage, &mut model)?;
        if usage.is_some() && model.is_some() {
          return Ok(rollout_usage(usage, model));
        }
      }
      end = newline;
    }
    if usage.is_some() && model.is_some() {
      return Ok(rollout_usage(usage, model));
    }
    if start == 0 {
      return Ok(rollout_usage(usage, model));
    }
    window.truncate(if end < window.len() { end + 1 } else { end });
    window_bytes = window_bytes.saturating_mul(2).max(INITIAL_ROLLOUT_TAIL_BYTES);
  }
}

fn inspect_rollout_line(
  line: &[u8],
  path: &Path,
  usage: &mut Option<UsageParts>,
  model: &mut Option<String>,
) -> anyhow::Result<()> {
  let Some(event) = parse_event_line(line, path, "codex_rollout_parse_skip")? else {
    return Ok(());
  };
  if usage.is_none() {
    *usage = usage_of_event(&event)?;
  }
  if model.is_none() {
    *model = model_of_event(&event)?;
  }
  Ok(())
}

fn rollout_usage(usage: Option<UsageParts>, model: Option<String>) -> Option<RolloutUsage> {
  usage.map(|(context_tokens, context_full, total_token_usage)| RolloutUsage {
    context_tokens,
    context_full,
    total_token_usage,
    model: model.unwrap_or_default(),
  })
}

/// The context tokens, the context window and the totals of a `token_count` event, or `None` when a count is missing.
fn usage_of_event(event: &Map<String, Value>) -> anyhow::Result<Option<UsageParts>> {
  let Some(payload) = token_count_payload(event)? else {
    return Ok(None);
  };
  let info = object_or_empty(payload, "info")?;
  let last = object_or_empty(info, "last_token_usage")?;
  // Codex reports the active prompt window in last_token_usage.input_tokens. total_token_usage is cumulative for the
  // session, and cached_input_tokens is a subset of the input, not an extra component of the window.
  let Some(input_tokens) = last.get("input_tokens").filter(|value| !value.is_null()) else {
    return Ok(None);
  };
  let Some(context_full) = info.get("model_context_window").filter(|value| !value.is_null()) else {
    return Ok(None);
  };
  let total = info.get("total_token_usage").and_then(Value::as_object).map(|total| {
    let mut totals = Map::new();
    for key in ["input_tokens", "cached_input_tokens", "output_tokens"] {
      totals.insert(key.to_string(), total.get(key).cloned().unwrap_or_else(|| json!(0)));
    }
    totals
  });
  Ok(Some((input_tokens.clone(), context_full.clone(), total)))
}

/// The model a `session_meta` or `turn_context` record names, when it names a non-empty one.
fn model_of_event(event: &Map<String, Value>) -> anyhow::Result<Option<String>> {
  if text_of(event, "type") != Some(TURN_CONTEXT) {
    return Ok(None);
  }
  let payload = object_or_empty(event, "payload")?;
  Ok(payload.get("model").and_then(Value::as_str).filter(|model| !model.is_empty()).map(str::to_string))
}

/// One line parsed as a record. A blank line and a line that is not JSON read as `None`; the latter is logged, as the
/// Python reader does. A line that is JSON but not an object is an error, as Python's attribute read on it is.
pub fn parse_event_line(line: &[u8], path: &Path, log_event: &str) -> anyhow::Result<Option<Map<String, Value>>> {
  if line.iter().all(u8::is_ascii_whitespace) {
    return Ok(None);
  }
  match serde_json::from_slice::<Value>(line) {
    Ok(Value::Object(event)) => Ok(Some(event)),
    Ok(other) => bail!("{}: rollout line is not a JSON object: {other}", path.display()),
    Err(error) => {
      tracing::debug!(target: "charliebot_agents", path = %path.display(), error = %error, "{log_event}");
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
  use std::io::Cursor;

  struct CountingReader {
    cursor: Cursor<Vec<u8>>,
    bytes_read: usize,
  }

  impl Read for CountingReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
      let count = self.cursor.read(buffer)?;
      self.bytes_read += count;
      Ok(count)
    }
  }

  impl Seek for CountingReader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
      self.cursor.seek(position)
    }
  }

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

  #[test]
  fn latest_usage_reads_only_the_tail_needed_for_the_model_and_count() {
    let filler = b"{}\n".repeat(1024 * 1024);
    let tail = concat!(
      "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5.5\"}}\n",
      "{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":10},\"model_context_window\":1000}}}\n"
    );
    let mut contents = filler;
    contents.extend_from_slice(tail.as_bytes());
    let mut reader = CountingReader { cursor: Cursor::new(contents), bytes_read: 0 };

    let usage = extract_latest_rollout_usage_from(&mut reader, Path::new("rollout.jsonl")).unwrap().unwrap();

    assert_eq!(usage.context_tokens, json!(10));
    assert_eq!(usage.model, "gpt-5.5");
    assert_eq!(reader.bytes_read, INITIAL_ROLLOUT_TAIL_BYTES);
  }

  #[test]
  fn truthy_non_object_payload_and_info_fail_instead_of_being_skipped() {
    let payload = json!({"type": EVENT_MSG, "payload": ["truthy"]});
    assert!(token_count_payload(payload.as_object().unwrap()).is_err());

    let info = json!({"type": EVENT_MSG, "payload": {"type": TOKEN_COUNT, "info": ["truthy"]}});
    assert!(usage_of_event(info.as_object().unwrap()).is_err());
  }
}
