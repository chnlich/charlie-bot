//! Test helper that plays the probe programs of the trigger tests.
//!
//! Cargo builds this binary once. Each fake program is a symlink named for
//! its role (for example `fake-ssh`) that points at this binary, next to a
//! `probe.json` data file that states the behavior of every role. The helper
//! reads its role from `argv[0]`, looks the role up in the data file, writes
//! the stated stdout and stderr, and exits with the stated code.
//!
//! A role's behavior is either one static answer (the fields `stdout`,
//! `stderr`, `exit`) or a `sequence` of such answers: each invocation
//! consumes the next entry and, past the end, repeats the last. Every
//! invocation appends one JSON array of its argv to `<role>.argv.jsonl` and
//! leaves its one-based invocation index in `<role>.calls`, so a test reads
//! back how many processes ran and with what remote command.

use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
  let argv: Vec<String> = std::env::args().collect();
  let argv0 = argv.first().expect("the helper always receives argv[0]");
  let link = resolve_link(argv0);
  let role = link.file_name().expect("argv[0] names the role").to_string_lossy().into_owned();
  let data_path = link.with_file_name("probe.json");
  let body = fs::read_to_string(&data_path)
    .unwrap_or_else(|error| panic!("read probe behavior {}: {error}", data_path.display()));
  let behavior: serde_json::Value =
    serde_json::from_str(&body).unwrap_or_else(|error| panic!("parse probe behavior {}: {error}", data_path.display()));
  let role_behavior =
    behavior.get(&role).unwrap_or_else(|| panic!("probe behavior {} states no role {role}", data_path.display()));
  let entry = sequence_entry(role_behavior, &link, &role);
  record_invocation(&argv, &link, &role);
  if role_behavior.get("run_remote").and_then(serde_json::Value::as_bool) == Some(true) {
    run_remote_command(&argv, &link);
  }
  let stdout = entry["stdout"].as_str().expect("probe behavior stdout is a string");
  let stdout = stdout.replace("$NOW_NS", &now_epoch_ns());
  let stderr = entry["stderr"].as_str().expect("probe behavior stderr is a string");
  let exit = entry["exit"].as_i64().expect("probe behavior exit is an integer");
  if !stdout.is_empty() {
    std::io::stdout().write_all(stdout.as_bytes()).expect("write fake stdout");
  }
  if !stderr.is_empty() {
    std::io::stderr().write_all(stderr.as_bytes()).expect("write fake stderr");
  }
  std::process::exit(i32::try_from(exit).expect("probe behavior exit fits i32"));
}

/// The path of the link this helper was started through. A shell that finds a fake by name on `PATH` passes that
/// bare name as argv[0], so the link is resolved the way the shell resolved it.
fn resolve_link(argv0: &str) -> PathBuf {
  let named = PathBuf::from(argv0);
  if named.components().count() > 1 {
    return named;
  }
  let paths = std::env::var_os("PATH").expect("a bare program name is found through PATH");
  std::env::split_paths(&paths)
    .map(|directory| directory.join(argv0))
    .find(|candidate| candidate.is_file())
    .unwrap_or_else(|| panic!("no fake program {argv0} on PATH"))
}

/// Run the remote command an ssh invocation names in a local `sh`, with the fake programs' directory first on its
/// `PATH`. That way the command's `sacct` is the fake of the same name. The held-session tests run their remote scripts
/// this way, against real local pids, and exit with the script's status. The remote home is the test's own home,
/// because a probe writes its end recorder and end file beneath `$HOME`; the runner's real home is no test's to use.
fn run_remote_command(argv: &[String], link: &Path) -> ! {
  let command = argv.last().expect("an ssh invocation names a remote command");
  let bin = link.parent().expect("a fake program sits in the fake bin directory");
  let remote_home = bin.parent().expect("the fake bin directory sits under the test's own home");
  let path = std::env::var("PATH").unwrap_or_default();
  let status = std::process::Command::new("/bin/sh")
    .arg("-c")
    .arg(command)
    .env("PATH", format!("{}:{path}", bin.display()))
    .env("HOME", remote_home)
    .status()
    .expect("run the remote command");
  std::process::exit(status.code().unwrap_or(1));
}

/// The answer this invocation gives: the next sequence entry, the last one past the end, or the static answer.
fn sequence_entry(role_behavior: &serde_json::Value, link: &Path, role: &str) -> serde_json::Value {
  let Some(sequence) = role_behavior.get("sequence") else {
    return role_behavior.clone();
  };
  let entries = sequence.as_array().expect("probe behavior sequence is a list");
  assert!(!entries.is_empty(), "probe behavior sequence for {role} is empty");
  let calls_path = link.with_file_name(format!("{role}.calls"));
  let index = fs::read_to_string(&calls_path).ok().and_then(|text| text.trim().parse::<usize>().ok()).unwrap_or(0);
  fs::write(&calls_path, format!("{}", index + 1)).expect("record the invocation index");
  entries.get(index).unwrap_or_else(|| entries.last().expect("the sequence is non-empty")).clone()
}

/// Append this invocation's argv as one JSON line, so a test reads back the remote command each call ran.
fn record_invocation(argv: &[String], link: &Path, role: &str) {
  let argv_path = link.with_file_name(format!("{role}.argv.jsonl"));
  let line = serde_json::to_string(argv).expect("serialize argv");
  let mut file = OpenOptions::new().create(true).append(true).open(&argv_path).expect("open argv log");
  file.write_all(format!("{line}\n").as_bytes()).expect("append argv log");
}

/// The helper host's clock, as `date +%s.%N` prints it. A sequence entry writes `$NOW_NS` where a real date line
/// belongs, so the offset math of the code under test measures a same-host clock.
fn now_epoch_ns() -> String {
  let now = SystemTime::now().duration_since(UNIX_EPOCH).expect("the system time is after the Unix epoch");
  format!("{}.{:09}", now.as_secs(), now.subsec_nanos())
}
