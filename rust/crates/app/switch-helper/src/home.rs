//! `home-helper`: the profile-home reads of the switch — the port and key, one run's process liveness, the outcome a
//! run recorded, and the quota end that accounts for a run which failed over. The live-run scan lives in `scan.rs`.
//! The reads share the same session layout.

use std::ffi::OsString;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use charliebot_cli_common::exit::CliExit;
use charliebot_core::config;
use charliebot_core::credentials;
use charliebot_core::event_types;
use charliebot_core::proc_identity::is_same_process;
use charliebot_core::run_kind::RunKind;
use charliebot_core::text_repr;
use charliebot_store::chat_events::{self, LineRule, LogReadError};
use charliebot_store::layout;
use charliebot_store::run_logs::{self, RUN_EVENTS_NAME};
use charliebot_store::run_records::{RUN_METADATA_NAME, read_run_metadata};
use serde_json::Value;

use crate::common::{USAGE, exact, failed, metadata_read_failure, read_config};
use crate::scan;

/// How long the outcome read waits for a manager turn's finish callback to persist its fact.
const MANAGER_TURN_OUTCOME_WAIT: Duration = Duration::from_secs(10);

pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  let Some((mode, rest)) = args.split_first() else {
    return Err(CliExit::usage(USAGE));
  };
  match mode.to_str() {
    Some("config") => config(rest),
    Some("switches") => switches(rest),
    Some("scan") => scan::scan(rest),
    Some("alive") => alive(rest),
    Some("outcome") => outcome(rest),
    Some("own-failure") => own_failure(rest),
    _ => Err(CliExit::usage(USAGE)),
  }
}

/// `config`: the server port on the first line and the access key on the second.
fn config(args: &[OsString]) -> Result<(), CliExit> {
  let [home] = exact::<1>(args)?;
  let home = Path::new(&home);
  let document = read_config(home).map_err(failed)?;
  let port = match document.get(config::SERVER_KEY).and_then(|server| server.get("port")) {
    Some(port) => text_repr::value_str(port),
    None => default_port().to_string(),
  };
  let credentials = credentials::load_credentials(home).map_err(|error| CliExit::error(error.to_string()))?;
  println!("{port}");
  println!("{}", credentials.access_key().unwrap_or_default());
  Ok(())
}

/// The port `ServerConfig` takes when `config.yaml` sets none: its serde default, which is the owner `default_port`.
fn default_port() -> u16 {
  serde_json::from_value::<config::ServerConfig>(Value::Object(serde_json::Map::new()))
    .expect("ServerConfig deserializes its defaults")
    .port
}

/// `switches`: `cpu=<true|false>` on the first line and `io=<true|false>` on the second, for
/// `server.enable_session_cpu_weight` and `server.enable_session_io_weight`. The section deserializes through
/// `ServerConfig`, so an absent switch reads as its default, the same value the server resolves.
fn switches(args: &[OsString]) -> Result<(), CliExit> {
  let [home] = exact::<1>(args)?;
  let home = Path::new(&home);
  let document = read_config(home).map_err(failed)?;
  print!("{}", switches_text(&document).map_err(failed)?);
  Ok(())
}

/// The `switches` output for one raw config document.
fn switches_text(document: &Value) -> anyhow::Result<String> {
  let (cpu, io) = config::session_weight_switches(document)?;
  Ok(format!("cpu={cpu}\nio={io}\n"))
}

/// `alive PID PID_START`: `alive` while the recorded process instance runs, `ended` once it is gone or recycled.
fn alive(args: &[OsString]) -> Result<(), CliExit> {
  let [pid, pid_start] = exact::<2>(args)?;
  let running = pid.parse::<u32>().is_ok_and(|pid| is_same_process(pid, &pid_start));
  println!("{}", if running { "alive" } else { "ended" });
  Ok(())
}

/// `reattach REPO SHA BACKEND_TYPE`: prints the resolved restart_reattach value (`true` or `false`) and the source
/// path.
pub(crate) fn reattach(args: &[OsString]) -> Result<(), CliExit> {
  let [repo, sha, backend_type] = exact::<3>(args)?;
  let (state, detail) = scan::read_registration(Path::new(&repo), &sha, &backend_type).map_err(failed)?;
  match state {
    scan::ReattachState::Unknown => Err(CliExit::error(detail)),
    scan::ReattachState::Enabled | scan::ReattachState::Disabled => {
      println!("{} ({detail})", state.text());
      Ok(())
    }
  }
}

/// `outcome HOME SESSION RUN`: the outcome the run recorded, as [`run_outcome`] reads it. The line is empty when no
/// fact names the run.
fn outcome(args: &[OsString]) -> Result<(), CliExit> {
  let [home, session_id, run_id] = exact::<3>(args)?;
  let session = config::sessions_dir_in(Path::new(&home)).join(&session_id);
  println!("{}", run_outcome(&session, &run_id)?);
  Ok(())
}

/// The outcome text of the run: the last outcome that a `run_finished` fact names for it, read from the session's
/// archive files and then its live event log. Empty when no fact names the run.
fn run_outcome(session: &Path, run_id: &str) -> Result<String, CliExit> {
  let metadata_path = layout::runs_dir(session).join(run_id).join(RUN_METADATA_NAME);
  let metadata =
    read_run_metadata(&metadata_path).map_err(|error| failed(metadata_read_failure(&metadata_path, error)))?;
  let manager_turn = metadata.get("kind").and_then(Value::as_str) == Some(RunKind::ManagerTurn.as_str());
  let mut outcome = Outcome::default();
  for archive in chat_events::archive_files(session).map_err(failed)? {
    for line in chat_events::read_lines(&archive, LineRule::Universal).map_err(failed)? {
      let line = line.map_err(failed)?;
      outcome.read(&line.text, line.number, &archive, run_id);
    }
  }
  let live = layout::chat_events_path(session);
  if live.is_file() {
    let text = chat_events::read_universal_text(&live).map_err(failed)?;
    let mut consumed = 0;
    let mut line_number = 0;
    drain(&text, &mut consumed, &mut line_number, &mut outcome, &live, run_id);
    if manager_turn && !outcome.recorded {
      // Startup queues manager-turn recovery in the background, so give its finish callback time to persist the fact.
      wait_for_outcome(&live, &mut consumed, &mut line_number, &mut outcome, run_id).map_err(failed)?;
    }
  }
  Ok(outcome.text())
}

/// `own-failure HOME SESSION RUN`: `quota SUCCESSOR` when the run ended failed on its own quota end and its session
/// continued in the run the server started for it. The line is empty otherwise.
///
/// The readback and check A of the switch both apply this rule to a run that ended before the stop. It reads only the
/// run's recorded facts. The outcome must be `failed`. The quota end is the run's last rate-limit record, when that
/// record is a rejection. The successor is the one run of the session that names the run as its retry and has started.
/// A run with no successor, or with no rejection of its own, is empty, and the switch counts it as died. A run with no
/// record of its own has no recorded failure either, so it is empty too.
fn own_failure(args: &[OsString]) -> Result<(), CliExit> {
  let [home, session_id, run_id] = exact::<3>(args)?;
  let session = config::sessions_dir_in(Path::new(&home)).join(&session_id);
  let recorded = layout::runs_dir(&session).join(&run_id).join(RUN_METADATA_NAME).is_file();
  let failed_run = recorded && run_outcome(&session, &run_id)? == "failed";
  let successor =
    if failed_run && quota_rejected(&session, &run_id)? { successor_run(&session, &run_id)? } else { None };
  match successor {
    Some(successor) => println!("quota {successor}"),
    None => println!(),
  }
  Ok(())
}

/// Whether the run's last rate-limit record rejects it. The backend writes its rate-limit records into the run's event
/// log, which the run-log compaction stores as `events.jsonl.zst` once the run is a day old, so the read resolves
/// either file. A run with no such record was not rejected.
fn quota_rejected(session: &Path, run_id: &str) -> Result<bool, CliExit> {
  let events = layout::runs_dir(session).join(run_id).join(RUN_EVENTS_NAME);
  let bytes = match run_logs::read_bytes(&events) {
    Ok(Some(bytes)) => bytes,
    Ok(None) => return Ok(false),
    Err(error) => return Err(failed(format!("read {}: {error}", events.display()))),
  };
  let needle = event_types::RATE_LIMIT_EVENT.as_bytes();
  let mut last_status = None;
  for (index, line) in bytes.split(|byte| *byte == b'\n').enumerate() {
    if !line.windows(needle.len()).any(|window| window == needle) {
      continue;
    }
    match chat_events::decode_line(line) {
      Ok(event) if event.get("type").and_then(Value::as_str) == Some(event_types::RATE_LIMIT_EVENT) => {
        last_status = event
          .get(event_types::RATE_LIMIT_INFO)
          .and_then(|info| info.get(event_types::RATE_LIMIT_STATUS))
          .and_then(Value::as_str)
          .map(str::to_string);
      }
      Ok(_) => {}
      Err(_) => eprintln!("skipped line {} of {}: not JSON", index + 1, events.display()),
    }
  }
  Ok(last_status.as_deref() == Some(event_types::RATE_LIMIT_STATUS_REJECTED))
}

/// The one run of the session that names `run_id` as its retry and has started. Two or more such runs name no
/// successor: the switch cannot tell which of them carries the work on, so the run counts as died.
fn successor_run(session: &Path, run_id: &str) -> Result<Option<String>, CliExit> {
  let mut successors = Vec::new();
  for entry in fs::read_dir(layout::runs_dir(session)).map_err(failed)? {
    let entry = entry.map_err(failed)?;
    let metadata_path = entry.path().join(RUN_METADATA_NAME);
    if !metadata_path.is_file() {
      continue;
    }
    let metadata =
      read_run_metadata(&metadata_path).map_err(|error| failed(metadata_read_failure(&metadata_path, error)))?;
    let retries = metadata.get("retry_of_run_id").and_then(Value::as_str) == Some(run_id);
    let started = metadata.get("started_at").is_some_and(|started| !started.is_null());
    if retries && started {
      successors.push(entry.file_name().to_string_lossy().into_owned());
    }
  }
  Ok(match successors.as_slice() {
    [only] => Some(only.clone()),
    _ => None,
  })
}

/// The complete live lines after `consumed`, with `consumed` advanced past them. A line still being written has no
/// newline and stays unread, as the live log's own reader requires.
fn drain(text: &str, consumed: &mut usize, line_number: &mut usize, outcome: &mut Outcome, path: &Path, run_id: &str) {
  for line in chat_events::take_complete_lines(text, consumed, line_number) {
    outcome.read(&line.text, line.number, path, run_id);
  }
}

/// Poll the live log for the run's finish fact, up to `MANAGER_TURN_OUTCOME_WAIT`.
fn wait_for_outcome(
  path: &Path,
  consumed: &mut usize,
  line_number: &mut usize,
  outcome: &mut Outcome,
  run_id: &str,
) -> Result<(), LogReadError> {
  let deadline = Instant::now() + MANAGER_TURN_OUTCOME_WAIT;
  while !outcome.recorded && Instant::now() < deadline {
    let text = match chat_events::read_universal_text(path) {
      Ok(text) => text,
      // A line the server is still writing can split a UTF-8 character. The next poll reads the whole line.
      Err(error) if error.kind() == ErrorKind::InvalidData => {
        thread::sleep(Duration::from_millis(100));
        continue;
      }
      Err(error) => return Err(error),
    };
    let before = *consumed;
    drain(&text, consumed, line_number, outcome, path, run_id);
    if *consumed == before {
      thread::sleep(Duration::from_millis(100));
    }
  }
  Ok(())
}

/// The facts one run's `run_finished` lines carry: the last outcome and whether any fact named the run.
#[derive(Default)]
struct Outcome {
  value: Option<Value>,
  recorded: bool,
}

impl Outcome {
  /// One event line, read into the outcome. A line the live log's own reader skips is not a fact. The skip is logged.
  fn read(&mut self, line: &str, number: usize, path: &Path, run_id: &str) {
    match chat_events::decode_line(line.as_bytes()) {
      Ok(event) => {
        if event.get("type").and_then(Value::as_str) == Some(event_types::RUN_FINISHED)
          && event.get("run_id").and_then(Value::as_str) == Some(run_id)
        {
          self.recorded = true;
          self.value = event.get("outcome").cloned();
        }
      }
      Err(_) => eprintln!("skipped line {number} of {}: not JSON", path.display()),
    }
  }

  /// `print(outcome or "")`: a falsy outcome reads as no outcome.
  fn text(&self) -> String {
    match &self.value {
      Some(value) if text_repr::is_truthy(value) => text_repr::value_str(value),
      _ => String::new(),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn switches_read_the_section_and_the_absent_ones_take_their_defaults() {
    let mut document = read_yaml("server:\n  enable_session_io_weight: true\n");
    assert_eq!(switches_text(&document).expect("the switches print"), "cpu=true\nio=true\n");
    document = read_yaml("server:\n  enable_session_io_weight: false\n");
    assert_eq!(switches_text(&document).expect("the switches print"), "cpu=true\nio=false\n");
    document = read_yaml("server:\n  port: 1\n");
    assert_eq!(switches_text(&document).expect("the switches print"), "cpu=true\nio=false\n");
  }

  /// One config.yaml text parsed the way `switches` reads it.
  fn read_yaml(text: &str) -> Value {
    let home = tempfile::tempdir().expect("a temp dir");
    std::fs::write(home.path().join(config::CONFIG_FILENAME), text).expect("write the fixture config");
    read_config(home.path()).expect("the fixture config reads")
  }

  #[test]
  fn the_last_run_finished_fact_wins_and_a_falsy_outcome_reads_empty() {
    let path = Path::new("events.jsonl");
    let mut outcome = Outcome::default();
    outcome.read(r#"{"type": "run_finished", "run_id": "r", "outcome": "failed"}"#, 1, path, "r");
    outcome.read(r#"{"type": "run_finished", "run_id": "r", "outcome": "success"}"#, 2, path, "r");
    outcome.read(r#"{"type": "run_finished", "run_id": "other", "outcome": "failed"}"#, 3, path, "r");
    assert!(outcome.recorded);
    assert_eq!(outcome.text(), "success");

    let mut null = Outcome::default();
    null.read(r#"{"type": "run_finished", "run_id": "r", "outcome": null}"#, 1, path, "r");
    assert!(null.recorded);
    assert_eq!(null.text(), "");
  }

  #[test]
  fn the_last_rate_limit_record_decides_a_quota_end_in_a_compacted_log() {
    let home = tempfile::tempdir().expect("a temp dir");
    let session = home.path().join("sessions").join("session");
    let run = layout::runs_dir(&session).join("run");
    fs::create_dir_all(&run).expect("the run directory");
    let rejected = "{\"type\":\"rate_limit_event\",\"rate_limit_info\":{\"status\":\"allowed\"}}\n\
                    {\"type\":\"rate_limit_event\",\"rate_limit_info\":{\"status\":\"rejected\"}}\n";
    let compressed = zstd::stream::encode_all(rejected.as_bytes(), 3).expect("compress the log");
    fs::write(run.join("events.jsonl.zst"), compressed).expect("the compacted log");
    assert!(quota_rejected(&session, "run").expect("the read"), "the compacted log holds the rejection");

    let allowed = "{\"type\":\"rate_limit_event\",\"rate_limit_info\":{\"status\":\"rejected\"}}\n\
                   {\"type\":\"rate_limit_event\",\"rate_limit_info\":{\"status\":\"allowed\"}}\n";
    let compressed = zstd::stream::encode_all(allowed.as_bytes(), 3).expect("compress the log");
    fs::write(run.join("events.jsonl.zst"), compressed).expect("the compacted log");
    assert!(!quota_rejected(&session, "run").expect("the read"), "the last record allows the run");
  }

  #[test]
  fn only_complete_live_lines_count() {
    let mut consumed = 0;
    let mut line_number = 0;
    let mut outcome = Outcome::default();
    let text = "{\"type\": \"run_finished\", \"run_id\": \"r\", \"outcome\": \"success\"}\n{\"type\": \"run_";
    drain(text, &mut consumed, &mut line_number, &mut outcome, Path::new("chat_events.jsonl"), "r");
    assert_eq!(line_number, 1);
    assert_eq!(outcome.text(), "success");
    assert_eq!(consumed, text.find('\n').expect("the first line ends") + 1);
  }
}
