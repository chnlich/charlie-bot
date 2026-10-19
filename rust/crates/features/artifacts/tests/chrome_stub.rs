//! The `chrome-stub` helper plays the fake headless Chrome of the unit tests. The unit tests find the helper in the target
//! directory, and only an integration test makes cargo build it there: this test names it through `CARGO_BIN_EXE_*`.

use std::fs;
use std::os::unix::fs::symlink;
use std::process::Command;

use serde_json::json;

#[test]
fn a_fake_plays_its_steps_in_order_and_exits_with_its_code() {
  let directory = tempfile::tempdir().expect("a temporary directory is available");
  let profile = directory.path().join("profile");
  fs::create_dir(&profile).expect("the profile directory is created");
  let fake = directory.path().join("chrome");
  symlink(env!("CARGO_BIN_EXE_chrome-stub"), &fake).expect("the fake links to the helper");
  let steps = json!([
    {"append": {"path": directory.path().join("log.txt").display().to_string(), "text": "launch\n"}},
    {"write_in_arg_dir": {"arg_prefix": "--user-data-dir=", "file": "DevToolsActivePort", "text": "9222\n"}},
    {"stdout": "out\n"},
    {"stderr": "err\n"},
    {"exit": 3},
  ]);
  fs::write(directory.path().join("probe.json"), json!({"chrome": steps}).to_string())
    .expect("the fake's steps are written");

  let output =
    Command::new(&fake).arg(format!("--user-data-dir={}", profile.display())).output().expect("the fake runs");

  assert_eq!(output.status.code(), Some(3));
  assert_eq!(String::from_utf8_lossy(&output.stdout), "out\n");
  assert_eq!(String::from_utf8_lossy(&output.stderr), "err\n");
  assert_eq!(fs::read_to_string(directory.path().join("log.txt")).expect("the log is written"), "launch\n");
  assert_eq!(fs::read_to_string(profile.join("DevToolsActivePort")).expect("the step wrote the file"), "9222\n");
}
