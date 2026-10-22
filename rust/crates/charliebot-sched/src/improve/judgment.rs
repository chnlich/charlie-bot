//! The mechanical judgments over an iteration: report validity, the summary text, and the quota blocker
//! (`src/features/improve/improve_command.py:250-287`, `:333-357`, `:494-583`).

use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::slice;

use anyhow::Context;
use charliebot_agents::backend_pool::QUOTA_ERROR_PATTERNS;
use charliebot_core::event_types::{ASSISTANT, ASSISTANT_ERROR, ERROR, RATE_LIMIT_EVENT, RATE_LIMIT_INFO, RESULT};
use charliebot_core::pyfmt::{PYTHON_JSON_DEFAULT, python_str};
use regex::Regex;
use serde_json::Value;

/// The heading that starts the previous-summaries section of an iteration description.
pub const ITERATION_SUMMARIES_HEADING: &str = "Previous iteration summaries:\n";
/// The first line of the placeholder report the controller writes when the worker wrote none.
pub const RUNNER_FALLBACK_REPORT_MARKER: &str = "<!-- runner fallback: worker wrote no report -->\n";
/// The error prefix of a loop whose worktree could not be created.
pub const WORKTREE_CREATE_ERROR_PREFIX: &str = "Failed to create worktree: ";
/// The error prefix of a loop whose controller raised.
pub const LOOP_FAILURE_ERROR_PREFIX: &str = "Improve loop failed: ";

/// The event types the newest-first judgments read: every other type is skipped unparsed.
const CANDIDATE_TYPES: [&str; 5] = [RESULT, ASSISTANT, ASSISTANT_ERROR, ERROR, RATE_LIMIT_EVENT];
const SUMMARY_TYPES: [&str; 2] = [RESULT, ASSISTANT];
const HEAD_PROOF_BYTES: usize = 256;
const REVERSE_SCAN_BYTES: usize = 64 * 1024;

/// Whether the report's `### Commits` section lists `none` as its first entry.
pub fn commits_section_first_none(text: &str) -> bool {
  let lines: Vec<&str> = text.lines().collect();
  let Some(heading) = lines.iter().position(|line| line.trim().starts_with("### Commits")) else {
    return false;
  };
  for entry in &lines[heading + 1..] {
    let stripped = entry.trim();
    if stripped.is_empty() {
      continue;
    }
    return stripped.strip_prefix("- ").is_some_and(|rest| starts_with_word_none(rest.trim()));
  }
  false
}

/// Python's `re.match(r"^none\b", text)`: `none` followed by a non-word character or the end.
fn starts_with_word_none(text: &str) -> bool {
  text.strip_prefix("none").is_some_and(|rest| rest.chars().next().is_none_or(|next| !is_word_char(next)))
}

fn is_word_char(character: char) -> bool {
  character.is_alphanumeric() || character == '_'
}

/// Judge an iteration's report: valid when it exists, carries its `## Iter <n>` heading, and either added commits or
/// states an explicit zero-progress verdict. Returns the invalid reason, or `None` when valid.
pub fn iter_report_validity(
  report_path: &Path,
  iteration: i64,
  commits_added: i64,
) -> anyhow::Result<(bool, Option<&'static str>)> {
  if !report_path.exists() {
    return Ok((false, Some("missing_report")));
  }
  let text = std::fs::read_to_string(report_path).with_context(|| format!("read {}", report_path.display()))?;
  let heading = Regex::new(&format!(r"(?m)^## Iter {iteration}\b")).expect("iteration heading expression is valid");
  if !heading.is_match(&text) {
    return Ok((false, Some("malformed_report")));
  }
  if commits_added > 0 || commits_section_first_none(&text) {
    return Ok((true, None));
  }
  Ok((false, Some("no_commit_no_verdict")))
}

/// The fixed one-line placeholder that stands in for an invalid iteration's summary.
pub fn invalid_iteration_summary(
  iteration: i64,
  reason: &str,
  commits_added: i64,
  tip_before: &str,
  tip_after: &str,
  report_path: &Path,
) -> String {
  format!(
    "Iteration {iteration} INVALID ({reason}); commits_added={commits_added}; tip {tip_before}..{tip_after}; report: {}",
    report_path.display()
  )
}

/// The text one event carries as a summary (at most 500 characters), or `None` when it carries none.
pub fn summary_text(event: &Value) -> Option<String> {
  match event.get("type").and_then(Value::as_str) {
    Some(RESULT) => event.get("result").and_then(Value::as_str).filter(|text| !text.is_empty()).map(take_500),
    Some(ASSISTANT) => {
      let text = message_text(event.get("message"));
      (!text.is_empty()).then(|| take_500(&text))
    }
    _ => None,
  }
}

fn take_500(text: &str) -> String {
  text.chars().take(500).collect()
}

/// The text blocks of an assistant message, joined in order (`message_aggregator.extract_text_from_message`).
fn message_text(message: Option<&Value>) -> String {
  let Some(blocks) = message.and_then(|message| message.get("content")).and_then(Value::as_array) else {
    return String::new();
  };
  blocks
    .iter()
    .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
    .filter_map(|block| block.get("text").and_then(Value::as_str))
    .collect()
}

/// The newest summary of an iteration's events, or the fixed no-summary sentence. The events arrive newest first.
pub fn extract_iteration_summary(events_newest_first: &[Value], iteration: i64, status: &str) -> String {
  events_newest_first
    .iter()
    .find_map(summary_text)
    .unwrap_or_else(|| format!("Iteration {iteration} {status} (no summary available)."))
}

/// Stream the newest summary from a reverse event walk.
pub fn extract_iteration_summary_from_events(
  events_newest_first: impl IntoIterator<Item = anyhow::Result<Value>>,
  iteration: i64,
  status: &str,
) -> anyhow::Result<String> {
  for event in events_newest_first {
    if let Some(summary) = summary_text(&event?) {
      return Ok(summary);
    }
  }
  Ok(format!("Iteration {iteration} {status} (no summary available)."))
}

/// The one pass over the newest-first events that settles both the quota blocker and the summary. The walk stops once
/// both are known.
pub fn failed_iteration_judgments(
  events_newest_first: &[Value],
  iteration: i64,
  status: &str,
) -> anyhow::Result<(Option<String>, String)> {
  failed_iteration_judgments_from_events(events_newest_first.iter().cloned().map(Ok), iteration, status)
}

/// The one pass over a streamed newest-first event walk that settles both the quota blocker and the summary.
pub fn failed_iteration_judgments_from_events(
  events_newest_first: impl IntoIterator<Item = anyhow::Result<Value>>,
  iteration: i64,
  status: &str,
) -> anyhow::Result<(Option<String>, String)> {
  let mut blocker_reason = None;
  let mut summary = None;
  for event in events_newest_first {
    let event = event?;
    if blocker_reason.is_none() {
      blocker_reason = quota_blocker_match(&event)?;
    }
    if summary.is_none() {
      summary = summary_text(&event);
    }
    if blocker_reason.is_some() && summary.is_some() {
      break;
    }
  }
  Ok((blocker_reason, summary.unwrap_or_else(|| format!("Iteration {iteration} {status} (no summary available)."))))
}

/// The quota blocker an event names, or `None`: a rate-limit rejection, a refused launch flagged as quota exhausted,
/// or an error whose text holds a quota pattern.
pub fn quota_blocker_match(event: &Value) -> anyhow::Result<Option<String>> {
  let event_type = event.get("type").and_then(Value::as_str);
  if event_type == Some(RATE_LIMIT_EVENT) {
    let info = event.get(RATE_LIMIT_INFO);
    let status = lowered_str(info.and_then(|info| info.get("status")));
    let overage_status = lowered_str(info.and_then(|info| info.get("overageStatus")));
    if status == "rejected" || overage_status == "rejected" {
      let rate_type = info
        .and_then(|info| info.get("rateLimitType"))
        .filter(|value| !is_python_falsy(value))
        .map_or_else(|| "rate limit".to_string(), python_str);
      return Ok(Some(format!("rate-limit rejection ({rate_type})")));
    }
  }

  // A refused launch states its cause on the run's error event; a False flag leaves the text patterns to judge.
  if event_type == Some(ERROR) && event.get(charliebot_core::event_types::QUOTA_EXHAUSTED) == Some(&Value::Bool(true)) {
    let message = event.get("message").context("the quota-exhausted error event has no message")?;
    return Ok(Some(format!("launch refused with quota exhausted: {}", python_str(message))));
  }

  if !matches!(event_type, Some(ERROR | ASSISTANT_ERROR | RESULT)) {
    return Ok(None);
  }
  let is_plain_result = event_type == Some(RESULT)
    && event.get("is_error") != Some(&Value::Bool(true))
    && event.get("api_error_status").is_none_or(Value::is_null)
    && !python_str(event.get("subtype").unwrap_or(&Value::String(String::new()))).to_lowercase().contains("error");
  if is_plain_result {
    return Ok(None);
  }

  let text = event_text(event).to_lowercase();
  Ok(
    QUOTA_ERROR_PATTERNS
      .iter()
      .find(|pattern| text.contains(*pattern))
      .map(|pattern| format!("provider quota/token/rate-limit rejection ({pattern})")),
  )
}

/// Python's `str(value).lower()` of an optional field; an absent field reads as `None`.
fn lowered_str(value: Option<&Value>) -> String {
  python_str(value.unwrap_or(&Value::Null)).to_lowercase()
}

/// Python's truthiness for the JSON values the rate-limit info carries.
fn is_python_falsy(value: &Value) -> bool {
  match value {
    Value::Null => true,
    Value::Bool(flag) => !flag,
    Value::Number(number) => number.as_f64() == Some(0.0),
    Value::String(text) => text.is_empty(),
    Value::Array(items) => items.is_empty(),
    Value::Object(members) => members.is_empty(),
  }
}

/// The text an event's fields carry, joined by newlines: the message's text blocks, and the other fields as text.
fn event_text(event: &Value) -> String {
  let mut parts = Vec::new();
  for key in ["message", "content", "result", "error", "api_error_status"] {
    let Some(value) = event.get(key).filter(|value| !value.is_null()) else { continue };
    if key == "message" && value.is_object() {
      let text = message_text(Some(value));
      if !text.is_empty() {
        parts.push(text);
      }
      continue;
    }
    match value {
      Value::Object(_) | Value::Array(_) => parts.push(charliebot_core::pyfmt::json_dumps(value, &PYTHON_JSON_DEFAULT)),
      other => parts.push(python_str(other)),
    }
  }
  parts.join("\n")
}

/// The iteration log's candidate events, newest first, as a compatibility collection.
pub fn newest_first_events(events_path: &Path) -> anyhow::Result<Vec<Value>> {
  newest_first_event_iter(events_path)?.collect()
}

/// The iteration log's candidate events, newest first. Python uses the same bounded head proof before parsing a line
/// (`src/infra/ndjson.py:404-468` and `src/features/improve/improve_command.py:575-587`).
pub fn newest_first_event_iter(events_path: &Path) -> anyhow::Result<NewestFirstEvents> {
  NewestFirstEvents::open(events_path, &CANDIDATE_TYPES)
}

/// Return the newest worker summary directly from the log, parsing only result and assistant events.
pub fn worker_summary_from_events_log(events_path: &Path) -> anyhow::Result<Option<String>> {
  for event in NewestFirstEvents::open(events_path, &SUMMARY_TYPES)? {
    let event = event?;
    if let Some(summary) = charliebot_tasks::review::worker_summary_from_events_log(std::slice::from_ref(&event)) {
      return Ok(Some(summary));
    }
  }
  Ok(None)
}

/// One reverse walk over an append-only NDJSON file. Irrelevant lines with a provable leading type cost one bounded
/// head read, while the iterator keeps no parsed event vector alive.
pub struct NewestFirstEvents {
  _file: Option<File>,
  mapping: Option<ReadOnlyMapping>,
  path: PathBuf,
  end: u64,
  keep_types: &'static [&'static str],
  bytes_probed: usize,
  bytes_parsed: usize,
  failed: bool,
}

struct ReadOnlyMapping {
  pointer: NonNull<u8>,
  length: usize,
}

impl ReadOnlyMapping {
  fn open(file: &File, path: &Path, length: usize) -> anyhow::Result<Option<Self>> {
    if length == 0 {
      return Ok(None);
    }
    // The event log is append-only while mapped, matching `src/infra/ndjson.py:_mapped_lines`.
    let mapping =
      unsafe { libc::mmap(std::ptr::null_mut(), length, libc::PROT_READ, libc::MAP_PRIVATE, file.as_raw_fd(), 0) };
    if mapping == libc::MAP_FAILED {
      return Err(std::io::Error::last_os_error()).with_context(|| format!("map {}", path.display()));
    }
    let pointer = NonNull::new(mapping.cast()).expect("a successful mmap returns a non-null pointer");
    Ok(Some(Self { pointer, length }))
  }

  fn bytes(&self) -> &[u8] {
    // SAFETY: the mapping covers `length` readable bytes and is unmapped only in `Drop`.
    unsafe { slice::from_raw_parts(self.pointer.as_ptr(), self.length) }
  }
}

impl Drop for ReadOnlyMapping {
  fn drop(&mut self) {
    let result = unsafe { libc::munmap(self.pointer.as_ptr().cast(), self.length) };
    if result != 0 {
      tracing::error!(error = %std::io::Error::last_os_error(), "ndjson_mmap_close_failed");
    }
  }
}

impl NewestFirstEvents {
  fn open(path: &Path, keep_types: &'static [&'static str]) -> anyhow::Result<Self> {
    let file = match File::open(path) {
      Ok(file) => Some(file),
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
      Err(error) => return Err(error).with_context(|| format!("open {}", path.display())),
    };
    let (mapping, end) = match &file {
      Some(file) => {
        let end = file.metadata().with_context(|| format!("read {} metadata", path.display()))?.len();
        let length = usize::try_from(end).with_context(|| format!("{} is too large to map", path.display()))?;
        (ReadOnlyMapping::open(file, path, length)?, end)
      }
      None => (None, 0),
    };
    Ok(Self {
      _file: file,
      mapping,
      path: path.to_path_buf(),
      end,
      keep_types,
      bytes_probed: 0,
      bytes_parsed: 0,
      failed: false,
    })
  }

  fn previous_line_range(&mut self) -> anyhow::Result<Option<(u64, u64)>> {
    let Some(mapping) = self.mapping.as_ref() else { return Ok(None) };
    let bytes = mapping.bytes();
    loop {
      if self.end == 0 {
        return Ok(None);
      }
      let line_end = self.end;
      let mut cursor = line_end;
      let mut newline = None;
      while cursor > 0 {
        let chunk_start = cursor.saturating_sub(REVERSE_SCAN_BYTES as u64);
        let chunk_start_usize = usize::try_from(chunk_start).expect("mapped file offset fits usize");
        let cursor_usize = usize::try_from(cursor).expect("mapped file offset fits usize");
        if let Some(offset) = bytes[chunk_start_usize..cursor_usize].iter().rposition(|byte| *byte == b'\n') {
          newline = Some(chunk_start + offset as u64);
          break;
        }
        cursor = chunk_start;
      }
      let line_start = newline.map_or(0, |position| position + 1);
      self.end = newline.unwrap_or(0);
      if line_start != line_end {
        return Ok(Some((line_start, line_end)));
      }
    }
  }

  fn next_event(&mut self) -> anyhow::Result<Option<Value>> {
    loop {
      let Some((start, end)) = self.previous_line_range()? else { return Ok(None) };
      let line_len = usize::try_from(end - start).expect("one NDJSON line fits usize");
      let Some(mapping) = self.mapping.as_ref() else { return Ok(None) };
      let bytes = mapping.bytes();
      let start = usize::try_from(start).expect("mapped file offset fits usize");
      let end = usize::try_from(end).expect("mapped file offset fits usize");
      let line = &bytes[start..end];
      let head_len = line_len.min(HEAD_PROOF_BYTES);
      let head = &line[..head_len];
      self.bytes_probed += head_len;
      if head.iter().all(u8::is_ascii_whitespace) && line_len <= HEAD_PROOF_BYTES {
        continue;
      }
      if head_proves_irrelevant_type(head, self.keep_types) {
        continue;
      }
      self.bytes_parsed += line_len;
      if line.iter().all(u8::is_ascii_whitespace) {
        continue;
      }
      let event: Value = match serde_json::from_slice(line) {
        Ok(event) => event,
        Err(error) => {
          tracing::warn!(target: "charliebot_sched", path = %self.path.display(), error = %error, "ndjson_parse_skip");
          continue;
        }
      };
      let Some(object) = event.as_object() else { continue };
      match object.get("type") {
        None | Some(Value::Null) => return Ok(Some(event)),
        Some(Value::String(kind)) if self.keep_types.contains(&kind.as_str()) => return Ok(Some(event)),
        Some(_) => {}
      }
    }
  }
}

impl Iterator for NewestFirstEvents {
  type Item = anyhow::Result<Value>;

  fn next(&mut self) -> Option<Self::Item> {
    if self.failed {
      return None;
    }
    match self.next_event() {
      Ok(Some(event)) => Some(Ok(event)),
      Ok(None) => None,
      Err(error) => {
        self.failed = true;
        Some(Err(error).context(format!("read {}", self.path.display())))
      }
    }
  }
}

fn head_proves_irrelevant_type(head: &[u8], keep_types: &[&str]) -> bool {
  let value = if let Some(rest) = head.strip_prefix(br#"{"type""#) {
    head_string_after_key(rest).map(|(value, _)| value)
  } else if let Some(rest) = head.strip_prefix(br#"{"id""#) {
    let Some((_, rest)) = head_string_after_key(rest) else { return false };
    let rest = rest.strip_prefix(b",").map(trim_head_whitespace).unwrap_or_default();
    let Some(rest) = rest.strip_prefix(br#""type""#) else { return false };
    head_string_after_key(rest).map(|(value, _)| value)
  } else {
    return false;
  };
  value.is_some_and(|value| !keep_types.iter().any(|kind| value == kind.as_bytes()))
}

fn head_string_after_key(mut rest: &[u8]) -> Option<(&[u8], &[u8])> {
  rest = trim_head_whitespace(rest);
  rest = rest.strip_prefix(b":")?;
  rest = trim_head_whitespace(rest);
  let rest = rest.strip_prefix(b"\"")?;
  let end = rest.iter().position(|byte| *byte == b'"')?;
  Some((&rest[..end], &rest[end + 1..]))
}

fn trim_head_whitespace(mut bytes: &[u8]) -> &[u8] {
  while matches!(bytes.first(), Some(b' ' | b'\t')) {
    bytes = &bytes[1..];
  }
  bytes
}

#[cfg(test)]
mod tests {
  use super::newest_first_event_iter;

  #[test]
  fn reverse_judgment_walk_parses_only_a_bounded_head_of_irrelevant_lines() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("events.jsonl");
    let summary = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"summary"}]}}"#;
    let irrelevant = format!(r#"{{"type":"tool_result","content":"{}"}}"#, "x".repeat(64 * 1024));
    std::fs::write(&path, format!("{summary}\n{irrelevant}\n")).unwrap();

    let mut events = newest_first_event_iter(&path).unwrap();
    let event = events.next().unwrap().unwrap();
    assert_eq!(event["type"], "assistant");
    assert_eq!(events.bytes_parsed, summary.len());
    assert_eq!(events.bytes_probed, 256 + summary.len());
    assert!(events.next().is_none());
  }
}
