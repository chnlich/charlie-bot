//! tools/improve/mock-home.py, run for real under `python3 -I`: one seed gives byte-identical output, and the event-log
//! sizes of a mid-size run follow the production shape. Each test writes its own output into a sandbox it owns.
//! The served checks of the same generator run in the release tests, against a trial server.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use charliebot_shell_harness::{self as harness, Sandbox};

fn mock_home() -> PathBuf {
  harness::repo_root().join("tools/improve/mock-home.py")
}

/// Runs mock-home.py generate into `output`, which must be empty. The run must succeed.
fn generate(sandbox: &Sandbox, output: &Path, sessions: &str, seed: &str) {
  let mut command = harness::command("python3", &sandbox.env());
  command.arg("-I").arg(mock_home()).arg("generate").arg(output).args(["--sessions", sessions, "--seed", seed]);
  let run = harness::run(&mut command);
  assert_eq!(run.code, 0, "{}", run.describe());
}

/// Every regular file under `root`, keyed by its path relative to `root`.
fn files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
  let mut found = BTreeMap::new();
  let mut pending = vec![root.to_path_buf()];
  while let Some(dir) = pending.pop() {
    for entry in fs::read_dir(&dir).expect("a directory") {
      let path = entry.expect("a directory entry").path();
      if path.is_dir() {
        pending.push(path);
      } else {
        let relative = path.strip_prefix(root).expect("a path under the root").to_path_buf();
        found.insert(relative, fs::read(&path).expect("a file"));
      }
    }
  }
  found
}

#[test]
fn test_generate_one_seed_gives_byte_identical_output_twice() {
  let sandbox = Sandbox::new();
  let first = sandbox.dir("first");
  let second = sandbox.dir("second");
  generate(&sandbox, &first, "2", "11");
  generate(&sandbox, &second, "2", "11");

  let first_files = files(&first);
  let second_files = files(&second);
  assert_eq!(first_files.keys().collect::<Vec<_>>(), second_files.keys().collect::<Vec<_>>());
  for (path, bytes) in &first_files {
    assert_eq!(second_files.get(path), Some(bytes), "{} differs between two runs of one seed", path.display());
  }
}

#[test]
fn test_generate_mid_size_event_logs_follow_the_production_shape() {
  let sandbox = Sandbox::new();
  let output = sandbox.dir("mid-size");
  generate(&sandbox, &output, "100", "5");

  let mut sizes: Vec<f64> = fs::read_dir(&output)
    .expect("the output directory")
    .map(|entry| entry.expect("a directory entry").path())
    .filter(|path| path.is_dir())
    .map(|session| fs::metadata(session.join("data/chat_events.jsonl")).expect("an event log").len() as f64)
    .collect();
  sizes.sort_by(f64::total_cmp);
  assert_eq!(sizes.len(), 100);
  let at = |quantile: f64| sizes[((sizes.len() - 1) as f64 * quantile).round() as usize];

  // A quantile of 100 sessions sits on one session, so each tolerance allows the error of that session's rank. The
  // maximum is the 144 MB knot, to within one event.
  assert_within("median", at(0.5), 1.7e3, 0.25);
  assert_within("p90", at(0.9), 1.4e6, 0.25);
  assert_within("p99", at(0.99), 1.7e7, 0.35);
  assert_within("maximum", at(1.0), 1.44e8, 0.01);
}

fn assert_within(name: &str, measured: f64, declared: f64, tolerance: f64) {
  let error = (measured - declared).abs() / declared;
  assert!(
    error <= tolerance,
    "{name}: measured {measured:.0} bytes, declared {declared:.0}, error {error:.3} > {tolerance}"
  );
}
