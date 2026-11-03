//! The `charliebot taste` binary surface: the help lists the five verbs and a row round-trips through the store.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

use serde_json::json;
use tempfile::TempDir;

struct Sandbox {
  _temp: TempDir,
  home: PathBuf,
}

impl Sandbox {
  fn new() -> Self {
    let temp = tempfile::tempdir().expect("the test temp directory is created");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("the CharlieBot home is created");
    Self { _temp: temp, home }
  }

  fn run(&self, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_charliebot"))
      .args(args)
      .env("CHARLIEBOT_HOME", &self.home)
      .output()
      .expect("the charliebot binary runs")
  }

  fn row_file(&self, row: &serde_json::Value) -> PathBuf {
    let path = self.home.join("row.json");
    fs::write(&path, row.to_string()).expect("the row file is written");
    path
  }
}

fn valid_row() -> serde_json::Value {
  json!({
    "type": "row",
    "decided_at": "2026-10-16T12:00:00+00:00",
    "domain": "work/code-review",
    "kind": "choice",
    "situation": "a plan fork: change the code or write it as a known limitation",
    "options": ["change the code", "write it as a known limitation"],
    "recommended": "change the code",
    "chosen": "write it as a known limitation",
    "reason": "未说明",
    "source": {"session_id": "sess-1", "url": "/?session=sess-1", "event_id": "evt-1"}
  })
}

fn row_with_comment_item(item: u64) -> serde_json::Value {
  let mut row = valid_row();
  row["situation"] = json!("a comment batch item: the comment batch answers several numbered items");
  row["source"]["comment_item"] = json!(item);
  row
}

fn stdout(output: &Output) -> String {
  String::from_utf8(output.stdout.clone()).expect("stdout is UTF-8")
}

fn stderr(output: &Output) -> String {
  String::from_utf8(output.stderr.clone()).expect("stderr is UTF-8")
}

#[test]
fn taste_help_lists_the_five_subcommands() {
  let sandbox = Sandbox::new();
  let output = sandbox.run(&["taste", "--help"]);
  assert!(output.status.success(), "{}", stderr(&output));
  let help = stdout(&output);
  for verb in ["add", "mark", "query", "index", "candidates"] {
    assert!(help.contains(verb), "the help lists {verb}:\n{help}");
  }
}

#[test]
fn a_row_round_trips_through_add_query_index_and_mark() {
  let sandbox = Sandbox::new();
  let path = sandbox.row_file(&valid_row());
  let path = path.to_str().expect("the row file path is UTF-8");
  let output = sandbox.run(&["taste", "add", "--file", path]);
  assert!(output.status.success(), "{}", stderr(&output));
  assert_eq!(stdout(&output).trim(), "T1");

  let output = sandbox.run(&["taste", "query", "--domain", "work/code-review"]);
  assert!(output.status.success(), "{}", stderr(&output));
  let query = stdout(&output);
  assert!(query.contains("T1"), "{query}");
  assert!(query.contains("write it as a known limitation"), "{query}");

  let output = sandbox.run(&["taste", "index"]);
  assert!(output.status.success(), "{}", stderr(&output));
  let index = stdout(&output);
  assert!(index.starts_with("Taste index:"), "{index}");
  assert!(index.ends_with("work/code-review"), "{index}");

  let output = sandbox.run(&["taste", "mark", "T1", "--covered-by", "memory:work/code-review/known-limits"]);
  assert!(output.status.success(), "{}", stderr(&output));
  let output = sandbox.run(&["taste", "query", "--domain", "work/code-review"]);
  assert!(!output.status.success(), "a covered row leaves no active row");
  assert!(stderr(&output).contains("unknown domain"), "{}", stderr(&output));

  let output = sandbox.run(&["taste", "add", "--file", path]);
  assert!(!output.status.success(), "a repeated source and chosen is refused");
  assert!(stderr(&output).contains("duplicate of T1"), "{}", stderr(&output));
}

#[test]
fn two_comment_items_of_one_event_land_and_a_rerun_of_one_is_refused() {
  let sandbox = Sandbox::new();
  let first = sandbox.row_file(&row_with_comment_item(3));
  let first = first.to_str().expect("the row file path is UTF-8");
  let output = sandbox.run(&["taste", "add", "--file", first]);
  assert!(output.status.success(), "{}", stderr(&output));
  assert_eq!(stdout(&output).trim(), "T1");

  let second = sandbox.row_file(&row_with_comment_item(5));
  let second = second.to_str().expect("the row file path is UTF-8");
  let output = sandbox.run(&["taste", "add", "--file", second]);
  assert!(
    output.status.success(),
    "two decisions from different items of one event both land despite the same chosen: {}",
    stderr(&output)
  );
  assert_eq!(stdout(&output).trim(), "T2");

  sandbox.row_file(&row_with_comment_item(3));
  let output = sandbox.run(&["taste", "add", "--file", first]);
  assert!(!output.status.success(), "a second extraction of one decision is refused");
  assert!(stderr(&output).contains("duplicate of T1"), "{}", stderr(&output));

  let output = sandbox.run(&["taste", "query", "--domain", "work/code-review", "--json"]);
  assert!(output.status.success(), "{}", stderr(&output));
  let rows: Vec<serde_json::Value> =
    stdout(&output).lines().map(|line| serde_json::from_str(line).expect("each queried row is JSON")).collect();
  let items: Vec<Option<u64>> = rows.iter().map(|row| row["source"]["comment_item"].as_u64()).collect();
  assert_eq!(items, [Some(3), Some(5)], "each row records its comment item: {rows:?}");
}

#[test]
fn a_row_without_a_comment_item_is_refused_when_added_twice() {
  let sandbox = Sandbox::new();
  let path = sandbox.row_file(&valid_row());
  let path = path.to_str().expect("the row file path is UTF-8");
  let output = sandbox.run(&["taste", "add", "--file", path]);
  assert!(output.status.success(), "{}", stderr(&output));
  let output = sandbox.run(&["taste", "add", "--file", path]);
  assert!(!output.status.success(), "a row with no item number keeps today's duplicate check");
  assert!(stderr(&output).contains("duplicate of T1"), "{}", stderr(&output));
}

#[test]
fn a_row_with_a_comment_item_lands_beside_an_older_row_of_the_same_event_without_one() {
  let sandbox = Sandbox::new();
  let plain = sandbox.row_file(&valid_row());
  let plain = plain.to_str().expect("the row file path is UTF-8");
  let output = sandbox.run(&["taste", "add", "--file", plain]);
  assert!(output.status.success(), "{}", stderr(&output));

  let numbered = sandbox.row_file(&row_with_comment_item(5));
  let numbered = numbered.to_str().expect("the row file path is UTF-8");
  let output = sandbox.run(&["taste", "add", "--file", numbered]);
  assert!(
    output.status.success(),
    "an item number never equals an absent one, so the row lands beside the older row: {}",
    stderr(&output)
  );
  assert_eq!(stdout(&output).trim(), "T2");
}
