//! The Claude Code `PreToolUse` hook for waits in Bash commands.
//!
//! A declared wait is the largest matching wait budget in one command: a sleep, a `tail --pid` wait, or a polling
//! loop with a sleep in its body. A `charliebot session trigger` command declares zero seconds outside an improve
//! round. Inside a round the guard refuses it: the round's host has no server to register a wake, so the round's
//! waits stay inside its run.
//!
//! A GitHub watch (`gh run watch`, or `gh pr checks --watch`) polls the REST API once per interval, so the guard
//! refuses a watch whose interval is below the minimum. A watch runs until its run ends, so it declares a wait of
//! unbounded length. The guard reads each command through the shared shell reader, and it reads a session trigger's
//! `--cmd` text as a command too.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::sync::OnceLock;

use charliebot_cli_common::exit::CliExit;
use charliebot_cli_common::improve_round;
use fancy_regex::{Captures, Regex};

use super::bash_hook::run_bash_guard;
use super::shell_commands::{Bodies, Invocation, base_name, invocations};

const WAIT_LIMIT_SECONDS: f64 = 60.0;
/// The shortest poll interval, in seconds, that a GitHub watch may set.
const MIN_WATCH_INTERVAL_SECONDS: u64 = 60;
const DENY_REASON: &str = "This command waits: it sleeps or repeats a check for more than 60 seconds. Run the work itself in the foreground in one call, or start the wait with `charliebot session trigger --cmd` and end your turn.";
/// The refusal inside an improve round, which has no CharlieBot server and no wake: a trigger cannot register, and a
/// wait stays inside the round's run.
const ROUND_DENY_REASON: &str = "An improve round has no CharlieBot server and no wake, so a session trigger cannot register, and a wait stays inside this run: re-check in calls of 60 seconds or less until the check passes.";
const WATCH_INTERVAL_DENY_REASON: &str = "Give the watch `--interval 120`, or another interval of 60 seconds or more. Every agent on this account shares one quota of 5,000 REST calls an hour, and a watch's default poll (3 seconds for `gh run watch`, 10 seconds for `gh pr checks --watch`) drains it.";
/// The refusal of a command that names `gh` and that the reader cannot parse. The guard cannot check its watches, so
/// it refuses the command, as the kill guard does for a command it cannot read.
const WATCH_UNREADABLE_DENY_REASON: &str = "This command names `gh` in a form the guard cannot read, so it cannot check the poll interval of a GitHub watch. Write each watch as a plain command with `--interval 120` or more: every agent on this account shares one quota of 5,000 REST calls an hour.";

/// Read one Claude Code hook event and emit a deny decision when its Bash command waits too long.
pub fn main(args: &[OsString]) -> Result<(), CliExit> {
  if !args.is_empty() {
    return Err(CliExit::usage("wait-guard takes no arguments"));
  }
  run(std::io::stdin().lock(), &mut std::io::stdout().lock())
}

fn run<R: Read, W: Write>(input: R, output: &mut W) -> Result<(), CliExit> {
  let in_round = improve_round::in_improve_round()?;
  run_bash_guard(input, output, |_event, command| refusal_reason(command, in_round))
}

/// The reason the guard refuses `command`, or `None` when the command runs. `in_round` is whether the process runs in
/// an improve round, where the guard also refuses a session trigger.
fn refusal_reason(command: &str, in_round: bool) -> Result<Option<&'static str>, String> {
  if let Some(reason) = watch_refusal(command) {
    return Ok(Some(reason));
  }
  if in_round && is_trigger_command(command)? {
    return Ok(Some(ROUND_DENY_REASON));
  }
  if declared_wait_seconds(command)? <= WAIT_LIMIT_SECONDS {
    return Ok(None);
  }
  Ok(Some(if in_round { ROUND_DENY_REASON } else { DENY_REASON }))
}

/// The refusal for a GitHub watch in `command` whose poll interval is below the minimum. A command that names `gh` and
/// that the reader cannot parse is refused too, since the guard cannot check its watches.
fn watch_refusal(command: &str) -> Option<&'static str> {
  if !command.contains("gh") {
    return None;
  }
  match watch_calls(command) {
    Ok(calls) if calls.iter().all(|words| interval_accepted(&words[3..])) => None,
    Ok(_) => Some(WATCH_INTERVAL_DENY_REASON),
    Err(_) => Some(WATCH_UNREADABLE_DENY_REASON),
  }
}

/// The GitHub watch commands that `command` runs, each as its words. Each session trigger's `--cmd` text is a command
/// that the trigger runs, so its watches count too.
fn watch_calls(command: &str) -> Result<Vec<Vec<String>>, String> {
  if !command.contains("gh") {
    return Ok(Vec::new());
  }
  let mut watches = Vec::new();
  for invocation in invocations(command, Bodies::Data, &["gh"])? {
    let Invocation::Call { words, .. } = invocation else {
      continue;
    };
    if let Some(trigger_text) = trigger_cmd(&words) {
      watches.extend(watch_calls(trigger_text)?);
    } else if is_watch(&words) {
      watches.push(words);
    }
  }
  Ok(watches)
}

/// The `--cmd` text of a `charliebot session trigger` command. clap reads the last `--cmd` it is given.
fn trigger_cmd(words: &[String]) -> Option<&str> {
  let [program, session, trigger, arguments @ ..] = words else {
    return None;
  };
  if base_name(program) != "charliebot" || session != "session" || trigger != "trigger" {
    return None;
  }
  let mut text = None;
  let mut rest = arguments.iter();
  while let Some(argument) = rest.next() {
    if argument == "--cmd" {
      text = rest.next().map(String::as_str);
    } else if let Some(value) = argument.strip_prefix("--cmd=") {
      text = Some(value);
    }
  }
  text
}

/// Whether `words` is a GitHub watch: `gh run watch`, or `gh pr checks` with `--watch` turned on.
fn is_watch(words: &[String]) -> bool {
  let [program, first, second, arguments @ ..] = words else {
    return false;
  };
  if base_name(program) != "gh" {
    return false;
  }
  match (first.as_str(), second.as_str()) {
    ("run", "watch") => true,
    ("pr", "checks") => watch_flag_set(arguments),
    _ => false,
  }
}

/// Whether a `gh pr checks` call turns on `--watch`: the bare flag, or `--watch=` with a true value.
fn watch_flag_set(arguments: &[String]) -> bool {
  arguments
    .iter()
    .take_while(|argument| argument.as_str() != "--")
    .any(|argument| argument == "--watch" || argument.strip_prefix("--watch=").is_some_and(is_true))
}

fn is_true(value: &str) -> bool {
  matches!(value, "1" | "t" | "T" | "TRUE" | "true" | "True")
}

/// Whether a watch's interval is the minimum or more. gh reads the last interval option it is given, and a watch with
/// no interval option polls at its default of 3 or 10 seconds, below the minimum.
fn interval_accepted(arguments: &[String]) -> bool {
  let last = interval_values(arguments).last().copied().flatten();
  last.and_then(|value| value.parse::<u64>().ok()).is_some_and(|seconds| seconds >= MIN_WATCH_INTERVAL_SECONDS)
}

/// The values of the interval options in a watch's arguments, in order. `None` marks an option with no value after it.
/// The options are `--interval`, `--interval=`, and `-i` in its forms `-i N`, `-iN` and `-i=N`, alone or in a cluster.
fn interval_values(arguments: &[String]) -> Vec<Option<&str>> {
  let mut values = Vec::new();
  let mut rest = arguments.iter();
  while let Some(argument) = rest.next() {
    match argument.as_str() {
      "--" => break,
      "--interval" | "-i" => values.push(rest.next().map(String::as_str)),
      text => {
        if let Some(value) = text.strip_prefix("--interval=") {
          values.push(Some(value));
        } else if let Some(cluster) = text.strip_prefix('-').filter(|_| !text.starts_with("--"))
          && let Some(at) = cluster.find('i')
        {
          let attached = &cluster[at + 1..];
          values.push(if attached.is_empty() {
            rest.next().map(String::as_str)
          } else {
            Some(attached.strip_prefix('=').unwrap_or(attached))
          });
        }
      }
    }
  }
  values
}

fn is_trigger_command(command: &str) -> Result<bool, String> {
  Ok(command.contains("charliebot") && trigger_command_pattern().is_match(command).map_err(regex_error)?)
}

fn declared_wait_seconds(command: &str) -> Result<f64, String> {
  if is_trigger_command(command)? {
    return Ok(0.0);
  }
  let mut wait = 0.0f64;
  if command.contains("tail") && command.contains("--pid") {
    for timeout in tail_pid_pattern().captures_iter(command) {
      let timeout = timeout.map_err(regex_error)?;
      wait = wait.max(match timeout.get(1) {
        Some(seconds) => seconds.as_str().parse::<f64>().expect("timeout pattern contains an integer"),
        None => f64::INFINITY,
      });
    }
  }
  let sleeps = if command.contains("sleep") { sleep_values(command)? } else { Vec::new() };
  for seconds in &sleeps {
    wait = wait.max(*seconds);
  }
  if ["for", "while", "until"].iter().any(|keyword| command.contains(keyword)) {
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
  }
  if !watch_calls(command)?.is_empty() {
    wait = f64::INFINITY;
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

fn trigger_command_pattern() -> &'static Regex {
  static PATTERN: OnceLock<Regex> = OnceLock::new();
  PATTERN.get_or_init(|| Regex::new(r"charliebot\s+session\s+trigger").expect("session trigger pattern is valid"))
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

  use serde_json::{Value, json};

  use super::*;

  fn run_input(input: &str) -> (Result<(), CliExit>, Vec<u8>) {
    let mut output = Vec::new();
    let result = run(Cursor::new(input.as_bytes()), &mut output);
    (result, output)
  }

  const WAIT_CASES: [(&str, bool); 15] = [
    ("for i in $(seq 1 84); do curl x; sleep 10; done", true),
    ("(nohup x &); sleep 420", true),
    ("timeout 880 tail --pid=96106 -f /dev/null", true),
    ("for i in 1 2 3 4 5 6; do out=$(timeout 40 charliebot web status show); sleep 10; done", false),
    ("for i in 1 2 3 4 5 6 7; do x; sleep 10; done", true),
    ("while x; do sleep 30; done", true),
    ("timeout 60 bash -c 'until x; do sleep 2; done'", false),
    ("sleep 20; grep -l x f | while read f; do python3 -c 1; done", false),
    ("for i in 1 2 3; do git fetch && break; sleep 5; done", false),
    ("charliebot session trigger --cmd 'while ! ok; do sleep 30; done' --max-wait 900 --message m", false),
    ("for s in a b c; do echo $s; done; sleep 30", false),
    ("for i in range(85): time.sleep(10)", true),
    ("for i in range(1, 85): time.sleep(10)", true),
    ("sleep 45; for s in a b; do python3 x; done", false),
    ("timeout 60 tail --pid=123 -f /dev/null", false),
  ];

  fn assert_wait_cases(cases: &[(&str, bool)]) {
    for (command, refuse) in cases {
      let wait = declared_wait_seconds(command).unwrap();
      assert_eq!(wait > WAIT_LIMIT_SECONDS, *refuse, "{command}");
    }
  }

  macro_rules! replay_case {
    ($name:ident, $index:literal) => {
      #[test]
      fn $name() {
        assert_wait_cases(&[WAIT_CASES[$index]]);
      }
    };
  }

  replay_case!(sequence_wait_budget_refuses_more_than_the_limit, 0);
  replay_case!(standalone_sleep_budget_refuses_more_than_the_limit, 1);
  replay_case!(tail_pid_budget_refuses_more_than_the_limit, 2);
  replay_case!(six_short_retries_stay_below_the_limit, 3);
  replay_case!(seven_short_retries_exceed_the_limit, 4);
  replay_case!(shell_poll_loop_uses_its_smallest_sleep, 5);
  replay_case!(timed_shell_poll_loop_stays_within_its_wrapper, 6);
  replay_case!(unbounded_sleep_before_read_loop_is_counted, 7);
  replay_case!(breaking_shell_loop_does_not_repeat_its_wait, 8);
  replay_case!(session_trigger_wait_is_not_counted, 9);
  replay_case!(sleep_after_loop_is_not_multiplied, 10);
  replay_case!(python_range_with_zero_start_counts_all_iterations, 11);
  replay_case!(python_range_with_nonzero_start_counts_all_iterations, 12);
  replay_case!(sleep_before_nonwaiting_loop_is_counted_once, 13);
  replay_case!(bounded_tail_pid_wait_stays_within_the_limit, 14);

  /// Each case keeps its verdict outside a round. Inside one, the guard also refuses every session trigger, and each
  /// refusal carries the in-run reason.
  #[test]
  fn each_wait_case_keeps_its_verdict_outside_a_round_and_refuses_the_trigger_inside_one() {
    for (command, refuse) in WAIT_CASES {
      let outside = refusal_reason(command, false).unwrap();
      assert_eq!(outside.is_some(), refuse, "{command}");
      if refuse {
        assert_eq!(outside, Some(DENY_REASON), "{command}");
      }
      let inside = refusal_reason(command, true).unwrap();
      let trigger = is_trigger_command(command).unwrap();
      assert_eq!(inside.is_some(), refuse || trigger, "{command}");
      if inside.is_some() {
        assert_eq!(inside, Some(ROUND_DENY_REASON), "{command}");
      }
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

  /// The interval rule refuses each GitHub watch that polls below the minimum. The command may run the watch plainly, in
  /// a compound command, behind a wrapper or a shell, in a substitution, or in a session trigger's `--cmd`.
  #[test]
  fn github_watch_below_the_minimum_interval_is_refused() {
    for command in [
      "gh run watch 123",
      "gh run watch 123 --interval 5",
      "gh run watch 123 -i 5",
      "gh run watch 123 --interval 120 --interval 5",
      "gh run watch 123 --interval",
      "gh pr checks 55 --watch",
      "gh pr checks 55 --watch --interval 30",
      "cd repo && gh run watch 123",
      "echo start; gh pr checks 55 --watch",
      "nohup gh run watch 123 > log 2>&1 &",
      "timeout 900 gh run watch 123",
      "bash -c 'gh run watch 123'",
      "x=$(gh run watch 123)",
      "charliebot session trigger --cmd 'gh run watch 1' --max-wait 900 --message m",
    ] {
      assert_eq!(watch_refusal(command), Some(WATCH_INTERVAL_DENY_REASON), "{command}");
    }
  }

  /// A watch whose interval is the minimum or more passes the interval rule, in each form of its interval option.
  #[test]
  fn github_watch_at_the_minimum_interval_passes_the_interval_rule() {
    for command in [
      "gh run watch 123 --interval 120",
      "gh run watch 123 --interval=60",
      "gh run watch 123 -i120",
      "gh run watch 123 -i=120",
      "gh run watch 123 --interval 5 --interval 120",
      "gh pr checks 55 --watch --interval 90",
      "gh pr checks 55 --watch=true --interval 90",
    ] {
      assert_eq!(watch_refusal(command), None, "{command}");
    }
  }

  /// A watch with an accepted interval is still a wait: it runs until its run ends, so the wait rule refuses it outside
  /// a session trigger, and the round rule refuses it inside a round.
  #[test]
  fn github_watch_with_an_accepted_interval_is_an_unbounded_wait() {
    let command = "gh run watch 123 --interval 120";
    assert_eq!(refusal_reason(command, false).unwrap(), Some(DENY_REASON));
    assert_eq!(refusal_reason(command, true).unwrap(), Some(ROUND_DENY_REASON));
  }

  /// A session trigger declares no wait, so outside a round it carries a watch whose interval is accepted.
  #[test]
  fn session_trigger_carries_a_watch_with_an_accepted_interval_outside_a_round() {
    let command = "charliebot session trigger --cmd 'gh run watch 1 --interval 120' --max-wait 900 --message m";
    assert_eq!(refusal_reason(command, false).unwrap(), None);
  }

  /// Text that only mentions a watch, and a gh call that is not a watch, pass the guard.
  #[test]
  fn text_and_non_watch_gh_calls_pass_the_guard() {
    for command in [
      r#"echo "gh run watch 1""#,
      "echo gh run watch 1",
      "grep -r 'gh run watch' dev",
      "gh pr checks 55",
      "gh pr checks 55 --interval 5",
      "gh run view 123 --json status",
    ] {
      assert_eq!(refusal_reason(command, false).unwrap(), None, "{command}");
    }
  }

  #[test]
  fn command_naming_gh_that_the_reader_cannot_read_is_refused() {
    assert_eq!(watch_refusal("echo \"gh run watch 1"), Some(WATCH_UNREADABLE_DENY_REASON));
  }

  #[test]
  fn hook_refuses_a_watch_below_the_minimum_interval_with_the_interval_reason() {
    let (result, output) = run_input(r#"{"tool_name":"Bash","tool_input":{"command":"gh run watch 123"}}"#);
    result.unwrap();
    let actual: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(actual["hookSpecificOutput"]["permissionDecisionReason"], WATCH_INTERVAL_DENY_REASON);
  }
}
