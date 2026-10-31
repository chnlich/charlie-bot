//! The `mock-home` program's generator, run for real: one seed gives byte-identical output, the event-log sizes of a
//! mid-size run follow the production shape, and every finished turn ends with a separator. Each test writes its own
//! output into a sandbox it owns. The served checks of the same generator run in the release tests, against a trial
//! server.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use charliebot_shell_harness::{self as harness, Sandbox};

fn mock_home() -> PathBuf {
  harness::built_binary("mock-home")
}

/// Runs `mock-home generate` into `output`, which must be empty. The run must succeed.
fn generate(sandbox: &Sandbox, output: &Path, sessions: &str, seed: &str) {
  let mut command = harness::command(mock_home(), &sandbox.env());
  command.arg("generate").arg(output).args(["--sessions", sessions, "--seed", seed]);
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

/// Whether an event is a plain user message rather than a tool result.
fn is_plain_user(event: &serde_json::Value) -> bool {
  event.get("type").and_then(serde_json::Value::as_str) == Some("user") && event.get("content").is_some()
}

#[test]
fn test_generate_separates_every_finished_turn_with_a_master_done() {
  let sandbox = Sandbox::new();
  let output = sandbox.dir("turns");
  generate(&sandbox, &output, "12", "5");

  let mut separators = 0;
  for entry in fs::read_dir(&output).expect("the output directory") {
    let session = entry.expect("a directory entry").path();
    if !session.is_dir() {
      continue;
    }
    let text = fs::read_to_string(session.join("data/chat_events.jsonl")).expect("an event log");
    let events: Vec<serde_json::Value> =
      text.lines().map(|line| serde_json::from_str(line).expect("one JSON event per line")).collect();
    assert!(is_plain_user(events.last().expect("a log ends with an event")), "a log ends with its checkpoint");
    for (index, event) in events.iter().enumerate() {
      if event.get("type").and_then(serde_json::Value::as_str) != Some("master_done") {
        continue;
      }
      separators += 1;
      assert!(index > 0 && !is_plain_user(&events[index - 1]), "a separator closes a turn with content");
      assert!(
        events.get(index + 1).is_some_and(is_plain_user),
        "a separator is followed by the user message that opens the next turn"
      );
      assert_eq!(event["exit_code"], 0);
      assert_eq!(event["still_thinking"], false);
      assert!(event["thinking_seconds"].is_number());
      assert!(event["timestamp"].is_string());
    }
  }
  assert!(separators > 0, "the generated home holds no turn separator");
}
