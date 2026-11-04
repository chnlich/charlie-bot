//! Collection: the failed runs whose `metadata.json` changed inside the window, and the server-log error lines stamped
//! inside it.
//!
//! A run counts as failed when its exit code is non-zero or its `run_finished` fact says `interrupted` with no
//! `run_stop_requested` fact. A run with a stop fact is a requested stop and is skipped. Log lines count only at the
//! `error` level, and their own stamp places them in the window.
//!
//! A failed run's events come from the log its record names: `events_ref`, else `result_ref`. A record that names
//! neither has no events, and a named log that is missing fails the scan.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context, Result, bail};
use charliebot_core::config::sessions_dir_in;
use charliebot_core::event_types;
use charliebot_core::logline;
use charliebot_core::pacific;
use charliebot_registry::backend::Env;
use charliebot_store::CompactValue;
use charliebot_store::chat_events::{self, LineRule};
use charliebot_store::layout::{chat_events_path, runs_dir};
use charliebot_store::run_records::{self, RUN_METADATA_NAME, STDERR_LOG_NAME, stop_requested_in_events};
use jiff::Timestamp;
use jiff::civil;
use jiff::tz::TimeZone;
use regex::Regex;

use crate::signature::{
  ExtractedCause, NoCauseShape, extract_run_cause, log_signature, normalize, redact, run_signature,
};

/// The bytes read from the end of a run's stderr log, matching the failure inventory's tail.
const STDERR_TAIL_BYTES: u64 = 8_000;
/// The number of examples one signature keeps.
const EXAMPLE_LIMIT: usize = 5;
/// The terminal outcome value a requested or recovered stop writes.
const INTERRUPTED_OUTCOME: &str = "interrupted";
/// The default server log directory, the launcher's `CHARLIEBOT_LOG_DIR` default.
pub const DEFAULT_LOG_DIR: &str = "/tmp/charliebot-logs";
/// The environment variable that moves the server log directory.
pub const LOG_DIR_ENV: &str = "CHARLIEBOT_LOG_DIR";

/// The window and the trees a scan reads.
#[derive(Clone, Debug)]
pub struct ScanOptions {
  pub home: PathBuf,
  pub log_dir: PathBuf,
  pub since: Timestamp,
  pub until: Timestamp,
}

impl ScanOptions {
  /// Build the window from the CLI's optional RFC 3339 bounds. `--until` defaults to now, and `--since` to seven days
  /// before it.
  pub fn new(home: PathBuf, log_dir: PathBuf, since: Option<&str>, until: Option<&str>) -> Result<ScanOptions, String> {
    let until = match until {
      Some(text) => {
        text.parse::<Timestamp>().map_err(|error| format!("--until is not an RFC 3339 timestamp: {error}"))?
      }
      None => Timestamp::now(),
    };
    let since = match since {
      Some(text) => {
        text.parse::<Timestamp>().map_err(|error| format!("--since is not an RFC 3339 timestamp: {error}"))?
      }
      None => until
        .checked_sub(jiff::SignedDuration::from_secs(7 * 24 * 60 * 60))
        .map_err(|error| format!("--until leaves no seven-day window: {error}"))?,
    };
    if since >= until {
      return Err("--since must be earlier than --until".to_string());
    }
    Ok(ScanOptions { home, log_dir, since, until })
  }

  /// Build the window from the two latest fires of `task` in the bound session's chat log.
  pub fn for_cron(home: PathBuf, log_dir: PathBuf, session_id: &str, task: &str) -> Result<ScanOptions> {
    let window = crate::window::cron_window(&home, session_id, task)?;
    Ok(ScanOptions { home, log_dir, since: window.since, until: window.until })
  }
}

/// Where a signature came from.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Source {
  Run,
  Log,
}

impl Source {
  /// The row's source text.
  pub fn as_str(self) -> &'static str {
    match self {
      Source::Run => "run",
      Source::Log => "log",
    }
  }
}

/// One signature with its count, its examples and the material the judge reads.
#[derive(Clone, Debug)]
pub struct CollectedSignature {
  pub signature: String,
  pub source: Source,
  /// The run kind, or `log` for a server-log line.
  pub kind: String,
  /// The normalized cause line that retrieval searches with; empty for a no-cause record.
  pub cause_line: String,
  /// The no-cause shape, when the record has no cause to judge.
  pub no_cause: Option<NoCauseShape>,
  pub count: usize,
  pub examples: Vec<String>,
  pub example_error: String,
}

#[derive(Debug)]
struct Group {
  signature: String,
  source: Source,
  kind: String,
  cause_line: String,
  no_cause: Option<NoCauseShape>,
  count: usize,
  examples: Vec<(Timestamp, String)>,
  example_error: String,
}

impl Group {
  fn new(signature: String, source: Source, kind: String, cause_line: String, no_cause: Option<NoCauseShape>) -> Group {
    Group {
      signature,
      source,
      kind,
      cause_line,
      no_cause,
      count: 0,
      examples: Vec::new(),
      example_error: String::new(),
    }
  }
}

/// Collect every signature inside `options`' window.
pub fn collect(options: &ScanOptions) -> Result<Vec<CollectedSignature>> {
  let mut groups: BTreeMap<String, Group> = BTreeMap::new();
  collect_runs(options, &mut groups)?;
  collect_logs(options, &mut groups)?;
  Ok(
    groups
      .into_values()
      .map(|group| {
        let mut examples = group.examples;
        examples.sort_by_key(|example| std::cmp::Reverse(example.0));
        examples.truncate(EXAMPLE_LIMIT);
        CollectedSignature {
          signature: group.signature,
          source: group.source,
          kind: group.kind,
          cause_line: group.cause_line,
          no_cause: group.no_cause,
          count: group.count,
          examples: examples.into_iter().map(|(_, text)| text).collect(),
          example_error: group.example_error,
        }
      })
      .collect(),
  )
}

/// The server log directory: `CHARLIEBOT_LOG_DIR` when set, else [`DEFAULT_LOG_DIR`].
pub fn log_dir_from_environment(environment: &Env) -> PathBuf {
  environment.get(LOG_DIR_ENV).map(PathBuf::from).unwrap_or_else(|| PathBuf::from(DEFAULT_LOG_DIR))
}

/// How many error events a run's event log is read for. The log is walked newest first, so a long log costs this many
/// events rather than its whole length.
const EVENT_LIMIT: usize = 200;

/// One run record's fields that collection reads.
struct RunFields {
  id: String,
  kind: String,
  exit_code: Option<i32>,
  backend: String,
  /// The event log the record names, or `None` when it names neither `events_ref` nor `result_ref`.
  event_log: Option<PathBuf>,
}

fn run_fields(path: &Path) -> Result<RunFields> {
  let value = run_records::read_run_metadata(path).map_err(|error| anyhow::anyhow!("{}: {error}", path.display()))?;
  let Some(id) = value.get("id").and_then(serde_json::Value::as_str) else {
    bail!("{} names no run id", path.display());
  };
  Ok(RunFields {
    id: id.to_string(),
    kind: value.get("kind").and_then(serde_json::Value::as_str).unwrap_or("work").to_string(),
    exit_code: value.get("exit_code").and_then(serde_json::Value::as_i64).map(|code| code as i32),
    backend: value.get("backend").and_then(serde_json::Value::as_str).unwrap_or("unknown").to_string(),
    event_log: named_event_log(&value),
  })
}

/// The event log a run record names: its `events_ref`, else its `result_ref`. The record's fields give the path, so no
/// file name is joined onto the run directory.
fn named_event_log(record: &serde_json::Value) -> Option<PathBuf> {
  ["events_ref", "result_ref"]
    .into_iter()
    .find_map(|key| record.get(key).and_then(serde_json::Value::as_str))
    .map(PathBuf::from)
}

/// The `run_finished` outcomes and `run_stop_requested` facts of one session.
#[derive(Debug, Default)]
struct SessionFacts {
  finished: HashMap<String, String>,
  stop: Vec<CompactValue>,
}

fn collect_runs(options: &ScanOptions, groups: &mut BTreeMap<String, Group>) -> Result<()> {
  let sessions_root = sessions_dir_in(&options.home);
  let sessions = match fs::read_dir(&sessions_root) {
    Ok(entries) => entries,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
    Err(error) => return Err(error).with_context(|| format!("reading {}", sessions_root.display())),
  };
  for session in sessions {
    let session_dir = session?.path();
    if !session_dir.is_dir() {
      continue;
    }
    let runs_root = runs_dir(&session_dir);
    let runs = match fs::read_dir(&runs_root) {
      Ok(entries) => entries,
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
      Err(error) => return Err(error).with_context(|| format!("reading {}", runs_root.display())),
    };
    let mut facts: Option<SessionFacts> = None;
    for run in runs {
      let run_dir = run?.path();
      if !run_dir.is_dir() {
        continue;
      }
      let record_file = run_dir.join(RUN_METADATA_NAME);
      let modified = match fs::metadata(&record_file).and_then(|metadata| metadata.modified()) {
        Ok(modified) => modified,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
        Err(error) => return Err(error).with_context(|| format!("stating {}", record_file.display())),
      };
      let instant = Timestamp::try_from(modified)
        .with_context(|| format!("reading the modification time of {}", record_file.display()))?;
      if instant < options.since || instant >= options.until {
        continue;
      }
      let record = run_fields(&record_file)?;
      if facts.is_none() {
        facts = Some(load_session_facts(&chat_events_path(&session_dir))?);
      }
      let facts = facts.as_ref().expect("the session facts were loaded above");
      if stop_requested_in_events(&facts.stop, &record.id, None) {
        continue;
      }
      let outcome = facts.finished.get(&record.id).map(String::as_str);
      let failed = record.exit_code.is_some_and(|code| code != 0) || outcome == Some(INTERRUPTED_OUTCOME);
      if !failed {
        continue;
      }
      let events = read_named_events(record.event_log.as_deref())?;
      let stderr = read_tail(&run_dir.join(STDERR_LOG_NAME), STDERR_TAIL_BYTES)?;
      let error_refs: Vec<&str> = events.errors.iter().map(String::as_str).collect();
      let cause =
        extract_run_cause(&error_refs, events.result_error.as_deref(), &stderr, events.terminal_reason.as_deref());
      let signature = run_signature(&record.kind, record.exit_code, &record.backend, &cause);
      let (cause_line, no_cause, anchor) = match &cause {
        ExtractedCause::Cause { line, anchor } => (normalize(line), None, anchor.clone()),
        ExtractedCause::NoCause { shape, anchor } => (String::new(), Some(*shape), anchor.clone()),
      };
      let group = groups
        .entry(signature.clone())
        .or_insert_with(|| Group::new(signature, Source::Run, record.kind, cause_line, no_cause));
      group.count += 1;
      if group.example_error.is_empty() {
        group.example_error = anchor;
      }
      group.examples.push((instant, record.id));
    }
  }
  Ok(())
}

fn load_session_facts(path: &Path) -> Result<SessionFacts> {
  let mut facts = SessionFacts::default();
  let lines = match chat_events::read_lines(path, LineRule::LossyLines) {
    Ok(lines) => lines,
    Err(chat_events::LogReadError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => return Ok(facts),
    Err(error) => return Err(anyhow::Error::new(error)).with_context(|| format!("reading {}", path.display())),
  };
  let finished = event_types::RUN_FINISHED;
  let stop = event_types::RUN_STOP_REQUESTED;
  for line in lines {
    let line = line.map_err(anyhow::Error::new).with_context(|| format!("reading {}", path.display()))?;
    if !line.text.contains(finished) && !line.text.contains(stop) {
      continue;
    }
    let Ok(event) = line.decode() else { continue };
    let Some(kind) = event.get("type").and_then(serde_json::Value::as_str) else { continue };
    if kind == finished {
      let Some(run_id) = event.get("run_id").and_then(serde_json::Value::as_str) else { continue };
      let Some(outcome) = event.get("outcome").and_then(serde_json::Value::as_str) else { continue };
      facts.finished.insert(run_id.to_string(), outcome.to_string());
    } else if kind == stop {
      facts.stop.push(CompactValue::from_value(&event));
    }
  }
  Ok(facts)
}

/// One run's cause material from its event log.
#[derive(Debug, Default)]
struct RunEvents {
  /// The `error` contents in file order.
  errors: Vec<String>,
  /// The error text the newest `result` event reports, when that event reports an error with text.
  result_error: Option<String>,
  /// The `terminal_reason` of the newest `result` event that carries one.
  terminal_reason: Option<String>,
}

/// The events of the log a run record names. A record that names no log has no events, and a named log that is missing
/// is an error that names the file.
fn read_named_events(log: Option<&Path>) -> Result<RunEvents> {
  let Some(path) = log else {
    return Ok(RunEvents::default());
  };
  match fs::metadata(path) {
    Ok(_) => read_run_events(path),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
      bail!("the run record names the event log {}, which is missing", path.display())
    }
    Err(error) => Err(error).with_context(|| format!("stating {}", path.display())),
  }
}

/// The events of one run event log. The log is walked newest first and the walk stops after [`EVENT_LIMIT`] error
/// events, so a long log costs no more than that. A line that is not JSON is skipped.
fn read_run_events(path: &Path) -> Result<RunEvents> {
  let mut run = RunEvents::default();
  let mut newest_result_seen = false;
  let mut events = run_records::NewestFirstEvents::open(path, &[event_types::ERROR, event_types::RESULT])?;
  for event in events.by_ref() {
    match event.get("type").and_then(serde_json::Value::as_str) {
      Some(event_types::ERROR) => {
        if let Some(content) = event.get("content").and_then(serde_json::Value::as_str) {
          run.errors.push(content.to_string());
        }
        if run.errors.len() >= EVENT_LIMIT {
          break;
        }
      }
      Some(event_types::RESULT) => {
        // The walk is newest first, so the first result event is the run's newest; only its error text is read.
        if !newest_result_seen {
          newest_result_seen = true;
          run.result_error = reported_error(&event);
        }
        // The first result that carries a reason is the newest one, as the inventory's file-order loop leaves the last
        // non-empty reason in place.
        if run.terminal_reason.is_none()
          && let Some(reason) = event.get("terminal_reason").and_then(serde_json::Value::as_str)
          && !reason.is_empty()
        {
          run.terminal_reason = Some(reason.to_string());
        }
      }
      _ => {}
    }
  }
  run.errors.reverse();
  Ok(run)
}

/// The text a `result` event reports as its error: its `result` text when `is_error` is true and the text is not empty.
fn reported_error(event: &serde_json::Value) -> Option<String> {
  if event.get("is_error").and_then(serde_json::Value::as_bool) != Some(true) {
    return None;
  }
  event.get("result").and_then(serde_json::Value::as_str).filter(|text| !text.is_empty()).map(str::to_string)
}

/// The last `max_bytes` of a file as text. A missing file reads as empty.
fn read_tail(path: &Path, max_bytes: u64) -> Result<String> {
  let mut file = match File::open(path) {
    Ok(file) => file,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
    Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
  };
  let length = file.metadata().with_context(|| format!("stating {}", path.display()))?.len();
  file
    .seek(SeekFrom::Start(length.saturating_sub(max_bytes)))
    .with_context(|| format!("seeking {}", path.display()))?;
  let mut bytes = Vec::new();
  file.read_to_end(&mut bytes).with_context(|| format!("reading {}", path.display()))?;
  Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// The stamp at the start of a text log line and the bytes it spans.
static TEXT_STAMP: LazyLock<Regex> = LazyLock::new(|| {
  Regex::new(r"^(\d{4}-\d{2}-\d{2})[ T](\d{2}:\d{2}:\d{2})(?:\.(\d+))?(Z?)").expect("the log stamp expression is valid")
});

fn collect_logs(options: &ScanOptions, groups: &mut BTreeMap<String, Group>) -> Result<()> {
  let entries = match fs::read_dir(&options.log_dir) {
    Ok(entries) => entries,
    Err(error) => return Err(error).with_context(|| format!("reading {}", options.log_dir.display())),
  };
  let mut files = Vec::new();
  for entry in entries {
    let path = entry?.path();
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else { continue };
    if !name.starts_with("server_") || !name.ends_with(".log") {
      continue;
    }
    files.push(path);
  }
  files.sort();
  for path in files {
    let file = File::open(&path).with_context(|| format!("reading {}", path.display()))?;
    let mut contents = String::new();
    let mut reader = std::io::BufReader::new(file);
    reader.read_to_string(&mut contents).with_context(|| format!("reading {}", path.display()))?;
    for line in contents.lines() {
      if !could_be_error(line) {
        continue;
      }
      let Some(record) = parse_log_line(line) else { continue };
      if record.instant < options.since || record.instant >= options.until {
        continue;
      }
      if record.level != "error" {
        continue;
      }
      let signature = log_signature(&record.event, &record.message);
      let group = groups
        .entry(signature.clone())
        .or_insert_with(|| Group::new(signature, Source::Log, "log".to_string(), normalize(&record.message), None));
      group.count += 1;
      if group.example_error.is_empty() {
        group.example_error = redact(&record.message).chars().take(crate::signature::MAX_ANCHOR_CHARS).collect();
      }
      group.examples.push((record.instant, record.line));
    }
  }
  Ok(())
}

/// A cheap line filter before the stamp and level parse.
fn could_be_error(line: &str) -> bool {
  let trimmed = line.trim_start();
  if trimmed.starts_with('{') {
    trimmed.contains("\"level\"") && trimmed.contains("\"error\"")
  } else {
    trimmed.contains("[error")
  }
}

struct LogRecord {
  instant: Timestamp,
  level: String,
  event: String,
  message: String,
  line: String,
}

fn parse_log_line(line: &str) -> Option<LogRecord> {
  let trimmed = line.trim();
  if trimmed.starts_with('{') {
    let value: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    let instant = value.get("timestamp")?.as_str()?.parse::<Timestamp>().ok()?;
    let row = logline::parse(trimmed)?;
    let message = log_message(&row);
    return Some(LogRecord {
      instant,
      level: logline::level(trimmed)?,
      event: row.event,
      message,
      line: trimmed.to_string(),
    });
  }
  let (instant, rest) = parse_text_stamp(trimmed)?;
  let canonical = format!("{}{rest}", canonical_stamp(instant));
  let row = logline::parse(&canonical)?;
  let message = log_message(&row);
  Some(LogRecord { instant, level: logline::level(&canonical)?, event: row.event, message, line: trimmed.to_string() })
}

fn log_message(row: &logline::Row) -> String {
  row.fields.iter().map(|(key, value)| format!("{key}={value}")).collect::<Vec<_>>().join(" ")
}

/// The instant of a text log stamp and the text after it. A stamp without `Z` is local Pacific time, as the launcher
/// wrote it before the server wrote UTC stamps.
fn parse_text_stamp(line: &str) -> Option<(Timestamp, &str)> {
  let caps = TEXT_STAMP.captures(line)?;
  let whole = caps.get(0)?;
  let date = caps.get(1)?.as_str();
  let time = caps.get(2)?.as_str();
  let fraction = caps.get(3).map(|part| part.as_str()).unwrap_or("0");
  let instant = if caps.get(4).is_some_and(|zone| zone.as_str() == "Z") {
    format!("{date}T{time}.{fraction}Z").parse::<Timestamp>().ok()?
  } else {
    let zone = pacific::zone().ok()?;
    let local = format!("{date}T{time}.{fraction}").parse::<civil::DateTime>().ok()?;
    zone.to_zoned(local).ok()?.timestamp()
  };
  Some((instant, &line[whole.end()..]))
}

fn canonical_stamp(instant: Timestamp) -> String {
  let moment = instant.to_zoned(TimeZone::UTC).datetime();
  format!(
    "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}Z",
    moment.year(),
    moment.month(),
    moment.day(),
    moment.hour(),
    moment.minute(),
    moment.second(),
    moment.subsec_nanosecond() / 1_000_000
  )
}

/// The repository's log directory must exist before a scan reads it.
pub fn require_log_dir(path: &Path) -> Result<()> {
  if !path.is_dir() {
    bail!("the server log directory {} does not exist", path.display());
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use charliebot_store::run_records::{RAW_LOG_NAME, RUN_EVENTS_NAME};
  use std::io::Write;
  use std::str::FromStr;
  use std::time::SystemTime;

  fn options(home: &Path, log_dir: &Path) -> ScanOptions {
    ScanOptions::new(
      home.to_path_buf(),
      log_dir.to_path_buf(),
      Some("2026-10-18T00:00:00-07:00"),
      Some("2026-11-02T00:00:00-08:00"),
    )
    .expect("the test window is valid")
  }

  /// Write one run's record, its files and its session facts. Events name their log in the record, as a worker does.
  fn write_run(
    home: &Path,
    session: &str,
    run_id: &str,
    metadata: &str,
    events: &str,
    stderr: &str,
    chat_events: &str,
  ) {
    let run_dir = home.join("sessions").join(session).join("data").join("runs").join(run_id);
    fs::create_dir_all(&run_dir).unwrap();
    let mut record: serde_json::Value = serde_json::from_str(metadata).unwrap();
    if !events.is_empty() {
      let events_path = run_dir.join(RUN_EVENTS_NAME);
      fs::write(&events_path, events).unwrap();
      record["events_ref"] = serde_json::json!(events_path.to_string_lossy());
    }
    fs::write(run_dir.join(RUN_METADATA_NAME), record.to_string()).unwrap();
    if !stderr.is_empty() {
      fs::write(run_dir.join(STDERR_LOG_NAME), stderr).unwrap();
    }
    let chat_path = home.join("sessions").join(session).join("data").join("chat_events.jsonl");
    let mut file = fs::OpenOptions::new().create(true).append(true).open(chat_path).unwrap();
    file.write_all(chat_events.as_bytes()).unwrap();
    let modified: SystemTime = Timestamp::from_str("2026-10-25T12:00:00Z").unwrap().into();
    File::options().write(true).open(run_dir.join(RUN_METADATA_NAME)).unwrap().set_modified(modified).unwrap();
  }

  #[test]
  fn a_window_whose_start_is_not_earlier_than_its_end_is_refused() {
    let error =
      ScanOptions::new(PathBuf::new(), PathBuf::new(), Some("2026-11-02T00:00:00Z"), Some("2026-11-02T00:00:00Z"))
        .unwrap_err();
    assert!(error.contains("earlier"), "{error}");
  }

  #[test]
  fn a_run_with_a_stop_fact_is_skipped_and_an_interrupted_null_exit_run_is_collected() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let log_dir = root.path().join("logs");
    fs::create_dir_all(&log_dir).unwrap();
    let stopped = r#"{"type":"run_stop_requested","run_id":"run-a"}"#;
    let finished = r#"{"type":"run_finished","run_id":"run-a","outcome":"interrupted"}"#;
    write_run(
      &home,
      "session",
      "run-a",
      r#"{"id":"run-a","kind":"work","exit_code":null,"backend":"synthetic"}"#,
      "",
      "",
      &format!("{stopped}\n{finished}\n"),
    );
    let collected = collect(&options(&home, &log_dir)).unwrap();
    assert!(collected.is_empty(), "{collected:?}");

    let recovered_finished = r#"{"type":"run_finished","run_id":"run-b","outcome":"interrupted"}"#;
    write_run(
      &home,
      "session",
      "run-b",
      r#"{"id":"run-b","kind":"work","exit_code":null,"backend":"synthetic"}"#,
      "",
      "",
      &format!("{recovered_finished}\n"),
    );
    let collected = collect(&options(&home, &log_dir)).unwrap();
    assert_eq!(collected.len(), 1);
    assert_eq!(collected[0].signature, "run:work:null:no-cause:synthetic");
    assert_eq!(collected[0].count, 1);
  }

  #[test]
  fn one_signature_counts_every_matching_run_and_keeps_five_examples() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let log_dir = root.path().join("logs");
    fs::create_dir_all(&log_dir).unwrap();
    let event = r#"{"type":"error","content":"the previous round still runs: unit improve-ux.scope exists"}"#;
    for index in 0..6 {
      let run_id = format!("run-{index}");
      write_run(
        &home,
        "session",
        &run_id,
        &format!(r#"{{"id":"{run_id}","kind":"work","exit_code":5,"backend":"synthetic"}}"#),
        event,
        "",
        &format!(r#"{{"type":"run_finished","run_id":"{run_id}","outcome":"failed"}}"#),
      );
    }
    let collected = collect(&options(&home, &log_dir)).unwrap();
    assert_eq!(collected.len(), 1);
    assert_eq!(collected[0].count, 6);
    assert_eq!(collected[0].examples.len(), 5);
    assert!(collected[0].cause_line.contains("improve-ux.scope"));
  }

  #[test]
  fn the_newest_result_event_supplies_the_terminal_reason() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join(RUN_EVENTS_NAME);
    fs::write(
      &path,
      concat!(
        r#"{"type":"result","terminal_reason":"old_reason"}"#,
        "\n",
        r#"{"type":"result"}"#,
        "\n",
        r#"{"type":"result","terminal_reason":"new_reason"}"#,
        "\n",
      ),
    )
    .unwrap();
    let events = read_run_events(&path).unwrap();
    assert!(events.errors.is_empty(), "{:?}", events.errors);
    assert_eq!(events.terminal_reason.as_deref(), Some("new_reason"));

    fs::write(
      &path,
      concat!(r#"{"type":"result","terminal_reason":"older_reason"}"#, "\n", r#"{"type":"result"}"#, "\n",),
    )
    .unwrap();
    let events = read_run_events(&path).unwrap();
    assert_eq!(events.terminal_reason.as_deref(), Some("older_reason"));
  }

  #[test]
  fn the_newest_result_event_supplies_its_error_text_only_when_it_reports_an_error() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join(RUN_EVENTS_NAME);
    fs::write(
      &path,
      concat!(
        r#"{"type":"result","is_error":true,"result":"older error"}"#,
        "\n",
        r#"{"type":"result","is_error":false,"result":"not an error"}"#,
        "\n",
      ),
    )
    .unwrap();
    assert_eq!(read_run_events(&path).unwrap().result_error, None);

    fs::write(
      &path,
      concat!(
        r#"{"type":"result","is_error":false,"result":"not an error"}"#,
        "\n",
        r#"{"type":"result","is_error":true,"result":"newest error"}"#,
        "\n",
      ),
    )
    .unwrap();
    assert_eq!(read_run_events(&path).unwrap().result_error.as_deref(), Some("newest error"));

    fs::write(&path, concat!(r#"{"type":"result","is_error":true}"#, "\n")).unwrap();
    assert_eq!(read_run_events(&path).unwrap().result_error, None);
  }

  #[test]
  fn a_failed_run_is_signed_by_its_result_error_not_the_backend_notice() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let log_dir = root.path().join("logs");
    fs::create_dir_all(&log_dir).unwrap();
    let notice = r#"{"type":"error","content":"[claude-code:unrecognized_model] {\"model\":\"deepseek-flash\"}"}"#;
    let result = concat!(
      r#"{"type":"result","is_error":true,"api_error_status":402,"terminal_reason":"api_error","#,
      r#""result":"API Error: 402 litellm.BadRequestError: OpenAIException - Insufficient Balance "#,
      r#"(request_id: 0a401b2c-3d4e-4f50-8a61-7b8c9d0e1f2a)\n\nLiteLLM: model group 'deepseek-flash' failed "#,
      r#"with the error above. No fallback was attempted."}"#,
    );
    write_run(
      &home,
      "session",
      "run-a",
      r#"{"id":"run-a","kind":"work","exit_code":1,"backend":"claude-code-deepseek-flash"}"#,
      &format!("{notice}\n{result}\n"),
      "",
      "",
    );
    let collected = collect(&options(&home, &log_dir)).unwrap();
    assert_eq!(collected.len(), 1, "{collected:?}");
    assert_eq!(
      collected[0].signature,
      "run:work:1:API Error: <n> litellm.BadRequestError: OpenAIException - Insufficient Balance \
       (request_id: <uuid>) LiteLLM: model group 'deepseek-flash' failed with the error above. \
       No fallback was attempted."
    );
  }

  #[test]
  fn a_run_whose_events_live_only_in_its_raw_log_is_signed_by_its_result_error() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let log_dir = root.path().join("logs");
    fs::create_dir_all(&log_dir).unwrap();
    let run_dir = home.join("sessions").join("session").join("data").join("runs").join("run-raw");
    let raw_path = run_dir.join(RAW_LOG_NAME);
    let metadata = format!(
      r#"{{"id":"run-raw","kind":"work","exit_code":1,"backend":"claude-code-improve-host3","result_ref":"{}"}}"#,
      raw_path.display()
    );
    let notice = r#"[claude-code:unrecognized_model] {"model":"synthetic-model","query_source":"sdk"}"#;
    write_run(&home, "session", "run-raw", &metadata, "", &format!("{notice}\n"), "");
    let raw = concat!(
      r#"{"type":"system","subtype":"init"}"#,
      "\n",
      "a line that is not JSON\n",
      r#"{"type":"result","is_error":true,"api_error_status":401,"#,
      r#""result":"Failed to authenticate. API Error: 401 Not allowed to access model"}"#,
      "\n",
      "a trailing line that is not JSON\n",
    );
    fs::write(&raw_path, raw).unwrap();
    let collected = collect(&options(&home, &log_dir)).unwrap();
    assert_eq!(collected.len(), 1, "{collected:?}");
    assert_eq!(collected[0].signature, "run:work:1:Failed to authenticate. API Error: <n> Not allowed to access model");
    assert!(collected[0].example_error.contains("401"), "{}", collected[0].example_error);
  }

  #[test]
  fn a_named_event_log_that_is_missing_is_an_error_that_names_the_file() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let log_dir = root.path().join("logs");
    fs::create_dir_all(&log_dir).unwrap();
    let missing = home.join("sessions").join("session").join("data").join("runs").join("run-a").join(RUN_EVENTS_NAME);
    let metadata = format!(
      r#"{{"id":"run-a","kind":"work","exit_code":1,"backend":"synthetic","events_ref":"{}"}}"#,
      missing.display()
    );
    write_run(&home, "session", "run-a", &metadata, "", "", "");
    let error = collect(&options(&home, &log_dir)).unwrap_err();
    assert!(format!("{error:#}").contains(&missing.display().to_string()), "{error:#}");
  }

  #[test]
  fn a_log_error_line_inside_the_window_becomes_its_own_signature() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let log_dir = root.path().join("logs");
    fs::create_dir_all(&log_dir).unwrap();
    fs::write(
      log_dir.join("server_20261025_120000.log"),
      concat!(
        "2026-10-25 19:00:00.000Z [error    ] tasks/recovery.rs:446        task_recovery_node_failed       ",
        "session=023536a4-48d6-5183-b9f9-8b588ffa0e43\n",
        "2026-10-25 19:00:01.000Z [info     ] tasks/recovery.rs:447        task_recovery_node_ok\n",
      ),
    )
    .unwrap();
    let collected = collect(&options(&home, &log_dir)).unwrap();
    assert_eq!(collected.len(), 1);
    assert_eq!(collected[0].source, Source::Log);
    assert_eq!(collected[0].signature, "log:task_recovery_node_failed:session=<session>");
  }
}
