//! The week-one review tool's JavaScript suites, `dev/week-review/test/*.test.mjs`, run under node's test runner. Each
//! suite builds its sessions and files in a temporary directory, so the run reads no session store and no model.

use std::path::PathBuf;
use std::process::Command;

use charliebot_shell_harness as harness;

fn suites() -> Vec<PathBuf> {
  let dir = harness::repo_root().join("dev/week-review/test");
  let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
    .expect("the test directory is readable")
    .map(|entry| entry.expect("a readable entry").path())
    .filter(|path| path.extension().is_some_and(|ext| ext == "mjs"))
    .collect();
  files.sort();
  assert!(!files.is_empty(), "no suite files under {}", dir.display());
  files
}

#[test]
fn the_review_tool_suites_pass() {
  let mut command = Command::new("node");
  command.arg("--test").args(suites());
  command.current_dir(harness::repo_root());
  let run = harness::run(&mut command);
  assert_eq!(run.code, 0, "{}", run.describe());
}
