//! The Claude Code `PreToolUse` hook for waits in Bash commands.
//!
//! A declared wait is the largest matching wait budget in one command: a sleep, a `tail --pid` wait, or a polling
//! loop with a sleep in its body. A `charliebot schedule-trigger` command declares zero seconds.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::sync::OnceLock;

use charliebot_cli_common::exit::CliExit;
use fancy_regex::{Captures, Regex};
use serde_json::{Value, json};

const WAIT_LIMIT_SECONDS: f64 = 60.0;
const DENY_REASON: &str = "This command waits: it sleeps or repeats a check for more than 60 seconds. Run the work itself in the foreground in one call, or start the wait with `charliebot schedule-trigger --cmd` and end your turn.";

/// Read one Claude Code hook event and emit a deny decision when its Bash command waits too long.
pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  if !args.is_empty() {
    return Err(CliExit::usage("wait-guard takes no arguments"));
  }
  run(std::io::stdin().lock(), &mut std::io::stdout().lock())
}

fn run<R: Read, W: Write>(mut input: R, output: &mut W) -> Result<(), CliExit> {
  let mut input_text = String::new();
  input
    .read_to_string(&mut input_text)
    .map_err(|error| CliExit::error(format!("could not read hook input: {error}")))?;
  let event: Value =
    serde_json::from_str(&input_text).map_err(|error| CliExit::error(format!("invalid hook JSON: {error}")))?;
  if event.get("tool_name").and_then(Value::as_str) != Some("Bash") {
    return Ok(());
  }
  let command = event
    .pointer("/tool_input/command")
    .and_then(Value::as_str)
    .ok_or_else(|| CliExit::error("Bash hook input must include a string tool_input.command"))?;
  let wait = declared_wait_seconds(command).map_err(CliExit::error)?;
  if wait > WAIT_LIMIT_SECONDS {
    serde_json::to_writer(&mut *output, &deny_decision())
      .map_err(|error| CliExit::error(format!("could not write hook output: {error}")))?;
    output.write_all(b"\n").map_err(|error| CliExit::error(format!("could not write hook output: {error}")))?;
  }
  Ok(())
}

fn deny_decision() -> Value {
  json!({
    "hookSpecificOutput": {
      "hookEventName": "PreToolUse",
      "permissionDecision": "deny",
      "permissionDecisionReason": DENY_REASON
    }
  })
}

fn declared_wait_seconds(command: &str) -> Result<f64, String> {
  if schedule_trigger_pattern().is_match(command).map_err(regex_error)? {
    return Ok(0.0);
  }
  let mut wait = 0.0f64;
  for timeout in tail_pid_pattern().captures_iter(command) {
    let timeout = timeout.map_err(regex_error)?;
    wait = wait.max(match timeout.get(1) {
      Some(seconds) => seconds.as_str().parse::<f64>().expect("timeout pattern contains an integer"),
      None => f64::INFINITY,
    });
  }
  let sleeps = sleep_values(command)?;
  for seconds in &sleeps {
    wait = wait.max(*seconds);
  }
  for header in loop_header_pattern().captures_iter(command) {
    let header = header.map_err(regex_error)?;
    let header_start = header.get(0).expect("loop pattern has a full match").start();
    let body_start = header.get(0).expect("loop pattern has a full match").end();
    let body_end = match loop_end_pattern().find(&command[body_start..]).map_err(regex_error)? {
      Some(end) => body_start + end.start(),
      None => command.len(),
    };
    let body_sleeps = sleep_values(&command[body_start..body_end])?;
    if body_sleeps.is_empty() {
      continue;
    }
    let smallest_sleep = body_sleeps.into_iter().fold(f64::INFINITY, f64::min);
    if let Some(iterations) = loop_iterations(&header) {
      wait = wait.max(iterations * smallest_sleep);
    } else {
      let mut wrapper_timeout = None::<f64>;
      for timeout in timeout_pattern().captures_iter(&command[..header_start]) {
        let timeout = timeout.map_err(regex_error)?;
        let seconds = timeout
          .get(1)
          .expect("timeout pattern has a seconds group")
          .as_str()
          .parse::<f64>()
          .expect("timeout pattern contains an integer");
        wrapper_timeout = Some(wrapper_timeout.map_or(seconds, |current| current.max(seconds)));
      }
      wait = wait.max(wrapper_timeout.unwrap_or(f64::INFINITY));
    }
  }
  Ok(wait)
}

fn loop_iterations(header: &Captures<'_>) -> Option<f64> {
  if let Some(count) = header.name("seq") {
    return Some(count.as_str().parse::<f64>().expect("loop count pattern contains an integer"));
  }
  if let Some(end) = header.name("rng") {
    return Some(end.as_str().parse::<f64>().expect("loop end pattern contains an integer"));
  }
  header.name("lst").map(|items| items.as_str().split_whitespace().count() as f64)
}

fn sleep_values(command: &str) -> Result<Vec<f64>, String> {
  sleep_pattern()
    .captures_iter(command)
    .map(|captures| {
      let captures = captures.map_err(regex_error)?;
      let seconds = captures
        .get(1)
        .or_else(|| captures.get(2))
        .expect("sleep pattern has a duration group")
        .as_str()
        .parse::<f64>()
        .expect("sleep pattern contains a decimal number");
      Ok(seconds)
    })
    .collect()
}

fn regex_error(error: fancy_regex::Error) -> String {
  format!("wait command parsing failed: {error}")
}

fn schedule_trigger_pattern() -> &'static Regex {
  static PATTERN: OnceLock<Regex> = OnceLock::new();
  PATTERN.get_or_init(|| Regex::new(r"charliebot\s+schedule-trigger").expect("schedule-trigger pattern is valid"))
}

fn tail_pid_pattern() -> &'static Regex {
  static PATTERN: OnceLock<Regex> = OnceLock::new();
  PATTERN.get_or_init(|| {
    Regex::new(r"(?:\btimeout\s+(\d+)\s+)?tail\s+(?:-f\s+)?--pid").expect("tail --pid pattern is valid")
  })
}

fn sleep_pattern() -> &'static Regex {
  static PATTERN: OnceLock<Regex> = OnceLock::new();
  PATTERN.get_or_init(|| {
    Regex::new(r"\bsleep\s+(\d+(?:\.\d+)?)|time\.sleep\((\d+(?:\.\d+)?)\)").expect("sleep pattern is valid")
  })
}

fn loop_header_pattern() -> &'static Regex {
  static PATTERN: OnceLock<Regex> = OnceLock::new();
  PATTERN.get_or_init(|| {
    Regex::new(concat!(
      r"\b(?P<kw>until|while(?!\s+(?:IFS=\S*\s+)?read\b))\b",
      r"|\bfor\s+\w+\s+in\s+(?P<it>\$\(seq\s+(?:\d+\s+)?(?P<seq>\d+)\)|",
      r"range\((?:\d+\s*,\s*)?(?P<rng>\d+)\)|(?P<lst>[^;$()]+?))\s*(?:;\s*do\b|:)"
    ))
    .expect("loop header pattern is valid")
  })
}

fn loop_end_pattern() -> &'static Regex {
  static PATTERN: OnceLock<Regex> = OnceLock::new();
  PATTERN.get_or_init(|| Regex::new(r"\bdone\b").expect("loop end pattern is valid"))
}

fn timeout_pattern() -> &'static Regex {
  static PATTERN: OnceLock<Regex> = OnceLock::new();
  PATTERN.get_or_init(|| Regex::new(r"\btimeout\s+(\d+)\b").expect("timeout pattern is valid"))
}

#[cfg(test)]
mod tests {
  use std::io::Cursor;

  use super::*;

  fn run_input(input: &str) -> (Result<(), CliExit>, Vec<u8>) {
    let mut output = Vec::new();
    let result = run(Cursor::new(input.as_bytes()), &mut output);
    (result, output)
  }

  #[test]
  fn declared_wait_cases_match_the_replay_table() {
    let cases = [
      ("for i in $(seq 1 84); do curl x; sleep 10; done", true),
      ("(nohup x &); sleep 420; tail log", true),
      ("timeout 880 tail --pid=96106 -f /dev/null", true),
      ("for i in 1 2 3 4 5 6; do out=$(timeout 40 charliebot web status show); sleep 10; done", false),
      ("for i in 1 2 3 4 5 6 7; do x; sleep 10; done", true),
      ("end=$(( $(date +%s) + 840 )); while [ $(date +%s) -lt $end ]; do timeout 10 x && break; sleep 30; done", true),
      (r#"timeout 60 bash -c "until curl ok; do sleep 2; done""#, false),
      ("sleep 20; grep -l x f | while read f; do python3 -c 1; done", false),
      ("for i in 1 2 3; do git fetch && break; sleep 5; done", false),
      ("charliebot schedule-trigger --cmd 'while ! ok; do sleep 30; done' --max-wait 900 --message m", false),
      ("for s in a b c; do echo $s; done; sleep 30", false),
      ("for i in range(85): time.sleep(10)", true),
      ("for i in range(1, 85): time.sleep(10)", true),
      ("sleep 45; for s in a b; do python3 x; done", false),
      ("timeout 60 tail --pid=123 -f /dev/null", false),
    ];
    for (command, refuse) in cases {
      let wait = declared_wait_seconds(command).unwrap();
      assert_eq!(wait > WAIT_LIMIT_SECONDS, refuse, "{command}");
    }
  }

  #[test]
  fn non_bash_hook_event_prints_nothing() {
    let (result, output) = run_input(r#"{"tool_name":"Read","tool_input":{}}"#);
    result.unwrap();
    assert!(output.is_empty());
  }

  #[test]
  fn invalid_json_returns_a_stderr_cli_error_without_a_deny_decision() {
    let (result, output) = run_input("not json");
    let error = result.unwrap_err();
    assert_eq!(error.code, 1);
    assert!(error.body["error"].as_str().unwrap().starts_with("invalid hook JSON:"));
    assert!(output.is_empty());
  }

  #[test]
  fn bash_event_without_a_string_command_returns_a_stderr_cli_error() {
    for input in [r#"{"tool_name":"Bash"}"#, r#"{"tool_name":"Bash","tool_input":{"command":61}}"#] {
      let (result, output) = run_input(input);
      let error = result.unwrap_err();
      assert_eq!(error.code, 1);
      assert!(error.body["error"].as_str().unwrap().contains("tool_input.command"));
      assert!(output.is_empty());
    }
  }

  #[test]
  fn refused_command_prints_the_exact_deny_json() {
    let (result, output) = run_input(r#"{"tool_name":"Bash","tool_input":{"command":"sleep 61"}}"#);
    result.unwrap();
    let actual: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(
      actual,
      json!({
        "hookSpecificOutput": {
          "hookEventName": "PreToolUse",
          "permissionDecision": "deny",
          "permissionDecisionReason": DENY_REASON
        }
      })
    );
  }
}
