//! Test helper that plays the probe programs of the trigger tests.
//!
//! Cargo builds this binary once. Each fake program is a symlink named for
//! its role (for example `fake-ssh`) that points at this binary, next to a
//! `probe.json` data file that states the behavior of every role. The helper
//! reads its role from `argv[0]`, looks the role up in the data file, writes
//! the stated stdout and stderr, and exits with the stated code.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

fn main() {
  let argv0 = std::env::args_os().next().expect("the helper always receives argv[0]");
  let link = PathBuf::from(argv0);
  let role = link.file_name().expect("argv[0] names the role").to_string_lossy().into_owned();
  let data_path = link.with_file_name("probe.json");
  let body = fs::read_to_string(&data_path)
    .unwrap_or_else(|error| panic!("read probe behavior {}: {error}", data_path.display()));
  let behavior: serde_json::Value =
    serde_json::from_str(&body).unwrap_or_else(|error| panic!("parse probe behavior {}: {error}", data_path.display()));
  let role_behavior =
    behavior.get(&role).unwrap_or_else(|| panic!("probe behavior {} states no role {role}", data_path.display()));
  let stdout = role_behavior["stdout"].as_str().expect("probe behavior stdout is a string");
  let stderr = role_behavior["stderr"].as_str().expect("probe behavior stderr is a string");
  let exit = role_behavior["exit"].as_i64().expect("probe behavior exit is an integer");
  if !stdout.is_empty() {
    std::io::stdout().write_all(stdout.as_bytes()).expect("write fake stdout");
  }
  if !stderr.is_empty() {
    std::io::stderr().write_all(stderr.as_bytes()).expect("write fake stderr");
  }
  std::process::exit(i32::try_from(exit).expect("probe behavior exit fits i32"));
}
