//! `home-helper`: the profile-home reads of the switch — the port and key, one run's process liveness, and the
//! outcome a run recorded. The live-run scan lives in `scan.rs`; the four read the same session layout.

use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use charliebot_cli_common::exit::CliExit;
use charliebot_core::config;
use charliebot_core::credentials;
use charliebot_core::event_types;
use charliebot_core::pyfmt;
use charliebot_store::chat_events::{self, LineRule, LogReadError};
use charliebot_store::layout;
use charliebot_store::run_records::{RUN_METADATA_NAME, read_run_metadata};
use serde_json::Value;

use crate::common::{USAGE, exact, failed, metadata_read_failure, python_truthy, read_config};
use crate::{process, scan};

/// How long the outcome read waits for a manager turn's finish callback to persist its fact.
const MANAGER_TURN_OUTCOME_WAIT: Duration = Duration::from_secs(10);

pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  let Some((mode, rest)) = args.split_first() else {
    return Err(CliExit::usage(USAGE));
  };
  match mode.to_str() {
    Some("config") => config(rest),
    Some("scan") => scan::scan(rest),
    Some("alive") => alive(rest),
    Some("outcome") => outcome(rest),
    _ => Err(CliExit::usage(USAGE)),
  }
}

/// `config`: the server port on the first line and the access key on the second.
fn config(args: &[OsString]) -> Result<(), CliExit> {
  let [home] = exact::<1>(args)?;
  let home = Path::new(&home);
  let document = read_config(home).map_err(failed)?;
  let port = match document.get("server").and_then(|server| server.get("port")) {
    Some(port) => pyfmt::python_str(port),
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

/// `alive PID PID_START`: `alive` while the recorded process instance runs, `ended` once it is gone or recycled.
fn alive(args: &[OsString]) -> Result<(), CliExit> {
  let [pid, pid_start] = exact::<2>(args)?;
  let running = pid.parse::<u32>().is_ok_and(|pid| process::instance(pid, &pid_start));
  println!("{}", if running { "alive" } else { "ended" });
  Ok(())
}

/// `outcome HOME SESSION RUN`: the outcome of the last `run_finished` fact for the run, from the session's archive
/// files and then its live event log; an empty line when no fact names the run.
fn outcome(args: &[OsString]) -> Result<(), CliExit> {
  let [home, session_id, run_id] = exact::<3>(args)?;
  let session = config::sessions_dir_in(Path::new(&home)).join(&session_id);
  let metadata_path = layout::runs_dir(&session).join(&run_id).join(RUN_METADATA_NAME);
  let metadata =
    read_run_metadata(&metadata_path).map_err(|error| failed(metadata_read_failure(&metadata_path, error)))?;
  let manager_turn = metadata.get("kind").and_then(Value::as_str) == Some("manager_turn");
  let mut outcome = Outcome::default();
  for archive in chat_events::archive_files(&session).map_err(failed)? {
    for line in chat_events::read_lines(&archive, LineRule::Universal).map_err(failed)? {
      let line = line.map_err(failed)?;
      outcome.read(&line.text, line.number, &archive, &run_id);
    }
  }
  let live = layout::chat_events_path(&session);
  if live.is_file() {
    let text = chat_events::read_universal_text(&live).map_err(failed)?;
    let mut consumed = 0;
    let mut line_number = 0;
    drain(&text, &mut consumed, &mut line_number, &mut outcome, &live, &run_id);
    if manager_turn && !outcome.recorded {
      // Startup queues manager-turn recovery in the background, so give its finish callback time to persist the fact.
      wait_for_outcome(&live, &mut consumed, &mut line_number, &mut outcome, &run_id).map_err(failed)?;
    }
  }
  println!("{}", outcome.text());
  Ok(())
}

/// Read the complete live lines after `consumed`, advancing it past them. A line still being written has no newline and
/// stays unread, as the live log's own reader requires.
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
      // A line the server is still writing can split a UTF-8 character; the next poll reads the whole line.
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
  /// Read one event line. A line the live log's own reader skips is not a fact; the skip is logged.
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
      Some(value) if python_truthy(value) => pyfmt::python_str(value),
      _ => String::new(),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

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
