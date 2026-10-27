//! Helpers that the manager turn (`master_run`) and the task run (`worker`) share: the stderr tail, the subprocess
//! buffer limit, the resume routing of a backend type, the extra flags of a launch, and the event channel of a runner.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, anyhow};
use charliebot_agents::runner::EventFn;
use charliebot_agents::transport::follow::SilenceCallback;
use charliebot_core::config::{BackendOption, CharlieBotConfig};
use charliebot_core::fsutil;
use charliebot_registry::backend::{RecoverySilence, ResumeStyle};
use charliebot_registry::context::FeatureContext;
use memmap2::Mmap;
use serde_json::{Map, Value};
use tokio::sync::{mpsc, oneshot};

const STDERR_TAIL_BYTES: u64 = 64 * 1024;
const TYPE_FILTER_HEAD_BYTES: usize = 256;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct EventScanStats {
  pub lines_visited: u64,
  pub events_parsed: u64,
}

/// One event handed from a runner to its consumer, with the channel the consumer acknowledges it on.
pub(crate) struct StreamEvent {
  pub event: Value,
  pub acknowledged: oneshot::Sender<Result<(), String>>,
}

/// The runner's event callback and the receiver its consumer reads. The callback returns only after the consumer
/// acknowledges the event, so the follow never passes an event the consumer has not processed. `stopped` and
/// `unacknowledged` are the errors of a consumer that has gone and of one that dropped its acknowledgement.
pub(crate) fn stream_event_channel(
  stopped: &'static str,
  unacknowledged: &'static str,
) -> (EventFn, mpsc::Receiver<StreamEvent>) {
  let (sender, receiver) = mpsc::channel::<StreamEvent>(1);
  let on_event: EventFn = Arc::new(move |event| {
    let sender = sender.clone();
    Box::pin(async move {
      let (ack_tx, ack_rx) = oneshot::channel();
      sender.send(StreamEvent { event, acknowledged: ack_tx }).await.map_err(|_| anyhow!(stopped))?;
      ack_rx.await.context(unacknowledged)?.map_err(anyhow::Error::msg)
    })
  });
  (on_event, receiver)
}

/// The last `STDERR_TAIL_BYTES` of a process's stderr log, trimmed; empty when the log does not exist.
pub(crate) async fn read_stderr_tail(path: &Path) -> anyhow::Result<String> {
  let path = path.to_path_buf();
  tokio::task::spawn_blocking(move || read_stderr_tail_file(&path).map(|(text, _)| text))
    .await
    .context("joining stderr tail reader")?
}

fn read_stderr_tail_file(path: &Path) -> anyhow::Result<(String, usize)> {
  let mut file = match File::open(path) {
    Ok(file) => file,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((String::new(), 0)),
    Err(error) => return Err(error).with_context(|| format!("reading stderr log {}", path.display())),
  };
  let len = file.metadata().with_context(|| format!("stating stderr log {}", path.display()))?.len();
  let start = len.saturating_sub(STDERR_TAIL_BYTES);
  file.seek(SeekFrom::Start(start)).with_context(|| format!("seeking stderr log {}", path.display()))?;
  let mut contents = Vec::with_capacity(usize::try_from(len - start).context("stderr tail size does not fit usize")?);
  file.read_to_end(&mut contents).with_context(|| format!("reading stderr tail {}", path.display()))?;
  let bytes_read = contents.len();
  Ok((String::from_utf8_lossy(&contents).trim().to_string(), bytes_read))
}

/// Visit matching NDJSON events newest first, stopping when `visit` returns a value. See [`NewestFirstEvents`].
pub(crate) fn visit_events_from_end<T>(
  path: &Path,
  keep_types: &[&str],
  visit: impl FnMut(Value) -> Option<T>,
) -> anyhow::Result<(Option<T>, EventScanStats)> {
  let mut events = NewestFirstEvents::open(path, keep_types)?;
  let value = events.by_ref().find_map(visit);
  Ok((value, events.stats))
}

/// The events of one NDJSON log, newest first, read through one read-only mapping. Only a provable mismatch in the
/// first 256 bytes skips a line without parsing it, matching Python's `iter_ndjson_events_from_end` and
/// `type_line_filter` (`src/infra/ndjson.py:404-547`). Each line's start is one backward `memrchr` from the previous
/// line's start, so a walk that stops at a line leaves the older bytes unscanned, as Python's mmap walk does.
pub struct NewestFirstEvents<'k> {
  map: Option<Mmap>,
  end: usize,
  keep_types: &'k [&'k str],
  pending: std::vec::IntoIter<Value>,
  stats: EventScanStats,
}

impl<'k> NewestFirstEvents<'k> {
  /// Map `path` for a walk of the events `keep_types` names; a missing file has no events.
  pub fn open(path: &Path, keep_types: &'k [&'k str]) -> anyhow::Result<Self> {
    let map = match fsutil::map_file(path) {
      Ok(map) => map,
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
      Err(error) => return Err(error).with_context(|| format!("opening event log {}", path.display())),
    };
    let end = map.as_ref().map_or(0, |map| map.len());
    Ok(Self { map, end, keep_types, pending: Vec::new().into_iter(), stats: EventScanStats::default() })
  }

  /// The byte range of the next non-empty line, newest first. Blank segments are skipped without a visit.
  fn next_line(&mut self) -> Option<(usize, usize)> {
    let bytes = self.map.as_deref().unwrap_or_default();
    while self.end > 0 {
      let end = self.end;
      let (start, next_end) = match memchr::memrchr(b'\n', &bytes[..end]) {
        Some(newline) => (newline + 1, newline),
        None => (0, 0),
      };
      self.end = next_end;
      if start < end {
        return Some((start, end));
      }
    }
    None
  }
}

impl Iterator for NewestFirstEvents<'_> {
  type Item = Value;

  fn next(&mut self) -> Option<Value> {
    loop {
      if let Some(event) = self.pending.next() {
        return Some(event);
      }
      let (start, end) = self.next_line()?;
      self.stats.lines_visited += 1;
      let bytes = self.map.as_deref().unwrap_or_default();
      let events = parse_kept_line(&bytes[start..end], self.keep_types);
      self.stats.events_parsed += events.len() as u64;
      self.pending = events.into_iter();
    }
  }
}

/// The line's parsed events, or none when its first bytes prove a type the reader does not keep.
fn parse_kept_line(line: &[u8], keep_types: &[&str]) -> Vec<Value> {
  let head = &line[..line.len().min(TYPE_FILTER_HEAD_BYTES)];
  if type_line_proves_rejection(head, keep_types) {
    return Vec::new();
  }
  charliebot_agents::transport::recovery::parse_raw_lines(line)
}

fn type_line_proves_rejection(head: &[u8], keep_types: &[&str]) -> bool {
  let rest = if let Some(rest) = head.strip_prefix(br#"{"type""#) {
    rest
  } else if let Some(rest) = head.strip_prefix(br#"{"id""#) {
    let Some((_, rest)) = first_header_string(rest) else { return false };
    let rest = trim_horizontal_space(rest);
    let Some(rest) = rest.strip_prefix(b",") else { return false };
    let rest = trim_horizontal_space(rest);
    let Some(rest) = rest.strip_prefix(br#""type""#) else { return false };
    rest
  } else {
    return false;
  };
  let Some((event_type, _)) = first_header_string(rest) else { return false };
  let event_type = String::from_utf8_lossy(event_type);
  !keep_types.contains(&event_type.as_ref())
}

fn first_header_string(mut rest: &[u8]) -> Option<(&[u8], &[u8])> {
  rest = trim_horizontal_space(rest);
  rest = rest.strip_prefix(b":")?;
  rest = trim_horizontal_space(rest);
  let quoted = rest.strip_prefix(b"\"")?;
  let end = quoted.iter().position(|byte| *byte == b'\"')?;
  Some((&quoted[..end], &quoted[end + 1..]))
}

fn trim_horizontal_space(mut bytes: &[u8]) -> &[u8] {
  while matches!(bytes.first(), Some(b' ' | b'\t')) {
    bytes = &bytes[1..];
  }
  bytes
}

/// The subprocess buffer limit of the configuration, in bytes.
pub(crate) fn subprocess_buffer_bytes(cfg: &CharlieBotConfig) -> anyhow::Result<usize> {
  let megabytes = usize::try_from(cfg.server.subprocess_buffer_limit_mb).context("negative subprocess buffer limit")?;
  megabytes.checked_mul(1024 * 1024).context("subprocess buffer limit overflows usize")
}

/// The flags and the native conversation id that resuming `cc_session_id` takes on `option`'s backend type.
pub(crate) fn route_resume_session(
  features: &FeatureContext,
  option: &BackendOption,
  cc_session_id: Option<&str>,
) -> anyhow::Result<(Vec<String>, Option<String>)> {
  let registration = features
    .registry
    .backend_type_of(&option.backend_type)
    .with_context(|| format!("unknown backend type {}", option.backend_type))?;
  route_resume_session_style(registration.traits.resume, cc_session_id)
}

pub(crate) fn route_resume_session_style(
  style: ResumeStyle,
  cc_session_id: Option<&str>,
) -> anyhow::Result<(Vec<String>, Option<String>)> {
  let Some(cc_session_id) = cc_session_id.filter(|id| !id.is_empty()) else { return Ok((Vec::new(), None)) };
  match style {
    ResumeStyle::CliFlag => Ok((vec!["--resume".to_string(), cc_session_id.to_string()], None)),
    ResumeStyle::NativeId => Ok((Vec::new(), Some(cc_session_id.to_string()))),
  }
}

/// The follow's silence callback for a recovered serve turn, from the deadline the serve transport reported.
pub(crate) fn silence_callback(silence: RecoverySilence) -> SilenceCallback {
  let message = silence.message;
  Box::new(move || {
    let message = message.clone();
    Box::pin(async move { Err(std::io::Error::new(std::io::ErrorKind::TimedOut, message)) })
  })
}

/// The `extra_flags` a lifecycle put into a launch's backend arguments; empty when it put none.
pub(crate) fn extra_flags_of(backend_kwargs: &Map<String, Value>) -> anyhow::Result<Vec<String>> {
  let Some(extra_flags) = backend_kwargs.get("extra_flags") else { return Ok(Vec::new()) };
  let extra_flags = extra_flags.as_array().context("lifecycle extra_flags must be an array")?;
  extra_flags
    .iter()
    .map(|flag| Ok(flag.as_str().context("lifecycle extra_flags entries must be strings")?.to_string()))
    .collect()
}

#[cfg(test)]
mod tests {
  use super::{read_stderr_tail_file, type_line_proves_rejection, visit_events_from_end};
  use std::fs;

  #[test]
  fn reverse_event_reader_parses_only_matching_summary_events() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("events.jsonl");
    let mut raw = String::from("{\"type\":\"result\",\"result\":\"done\"}\n");
    for index in 0..256 {
      raw.push_str(&format!("{{\"type\":\"user\",\"id\":\"tool-{index}\",\"blob\":\"{}\"}}\n", "x".repeat(1024)));
    }
    assert!(type_line_proves_rejection(br#"{"type":"user","id":"tool"}"#, &["result", "assistant"]));
    fs::write(&path, raw).unwrap();

    let (summary, stats) = visit_events_from_end(&path, &["result", "assistant"], |event| {
      event.get("result").and_then(serde_json::Value::as_str).map(str::to_string)
    })
    .unwrap();
    assert_eq!(summary.as_deref(), Some("done"));
    assert_eq!(stats.events_parsed, 1, "filtered lines are not parsed into Value objects");
    assert_eq!(stats.lines_visited, 257);
  }

  #[test]
  fn reverse_event_reader_stops_at_the_newest_matching_event() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("events.jsonl");
    let mut raw = String::new();
    for index in 0..200 {
      raw.push_str(&format!("{{\"type\":\"user\",\"id\":\"tool-{index}\",\"blob\":\"{}\"}}\n", "x".repeat(1024)));
    }
    raw.push_str("{\"type\":\"result\",\"result\":\"done\"}\n");
    fs::write(&path, &raw).unwrap();

    let (summary, stats) = visit_events_from_end(&path, &["result"], |event| {
      event.get("result").and_then(serde_json::Value::as_str).map(str::to_string)
    })
    .unwrap();
    assert_eq!(summary.as_deref(), Some("done"));
    assert_eq!(stats.lines_visited, 1, "the 200 padding lines before the result are never visited");
    assert_eq!(stats.events_parsed, 1);
  }

  #[test]
  fn reverse_event_reader_visits_each_nonblank_line_newest_first_across_pages() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("events.jsonl");
    let mut raw = String::new();
    let mut forward = Vec::new();
    for index in 0..120 {
      let blob = "x".repeat(index * 97 % 9000);
      raw.push_str(&format!("{{\"type\":\"user\",\"id\":\"line-{index}\",\"blob\":\"{blob}\"}}\n"));
      forward.push(format!("line-{index}"));
      if index % 5 == 0 {
        raw.push('\n');
      }
    }
    raw.push_str("{\"type\":\"result\",\"id\":\"last\"}");
    forward.push("last".to_string());
    fs::write(&path, &raw).unwrap();

    let mut newest_first = Vec::new();
    let (_, stats) = visit_events_from_end(&path, &["user", "result"], |event| {
      newest_first.push(event["id"].as_str().unwrap().to_string());
      None::<()>
    })
    .unwrap();
    forward.reverse();
    assert_eq!(newest_first, forward);
    assert_eq!(stats.lines_visited, forward.len() as u64);
  }

  #[test]
  fn stderr_tail_reads_at_most_sixty_four_kibibytes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("stderr.log");
    let mut raw = vec![b'x'; 1024 * 1024];
    raw.extend_from_slice(b" keep this tail \n");
    fs::write(&path, raw).unwrap();

    let (tail, bytes_read) = read_stderr_tail_file(&path).unwrap();
    assert_eq!(bytes_read, 64 * 1024);
    assert!(tail.ends_with("keep this tail"));
  }
}
