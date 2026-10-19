//! Parses the canned `ncu --csv --page details` outputs of a manifest with the native CSV path and prints one JSON
//! object that maps each case's name to its result.
//!
//! `tests/csv_differential.py` runs this over `tests/canned/cases.json` and compares the results with Python's
//! `_parse_with_csv`. Usage: `csv_probe <cases.json>`. A case's `stdout_file` is read next to the manifest exactly as
//! it is written, because the differential feeds the same bytes to both parsers.

use std::fs;
use std::path::{Path, PathBuf};

use charliebot_ncu::csv_import::{self, Outcome};
use charliebot_ncu::error::ParseError;
use serde::Deserialize;
use serde_json::{Map, Value, json};

#[derive(Deserialize)]
struct Manifest {
  cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
  name: String,
  #[serde(flatten)]
  run: Run,
}

/// What the `ncu` run did, as `csv_import::Outcome` sees it.
#[derive(Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum Run {
  Completed { success: bool, stdout_file: String, stderr: String },
  NotFound,
  Timeout,
}

fn main() {
  let manifest_path = PathBuf::from(std::env::args_os().nth(1).expect("usage: csv_probe <cases.json>"));
  let text = fs::read_to_string(&manifest_path).expect("read the manifest");
  let manifest: Manifest = serde_json::from_str(&text).expect("parse the manifest");
  let dir = manifest_path.parent().expect("the manifest is in a directory");

  let mut results = Map::new();
  for case in manifest.cases {
    let outcome = outcome(case.run, dir);
    results.insert(case.name, result_json(csv_import::parse(outcome)));
  }
  println!("{}", Value::Object(results));
}

fn outcome(run: Run, dir: &Path) -> Outcome {
  match run {
    Run::Completed { success, stdout_file, stderr } => {
      let stdout = fs::read_to_string(dir.join(&stdout_file)).expect("read a canned stdout");
      Outcome::Completed { success, stdout, stderr }
    }
    Run::NotFound => Outcome::NotFound,
    Run::Timeout => Outcome::TimedOut,
  }
}

/// The shape `csv_differential.py` builds from Python's result: a report, a message, or an internal error's kind.
fn result_json(result: Result<Value, ParseError>) -> Value {
  match result {
    Ok(report) => json!({"status": "ok", "report": report}),
    Err(ParseError::Report(message)) => json!({"status": "report_error", "message": message}),
    Err(ParseError::Internal { kind, detail }) => json!({"status": "internal_error", "kind": kind, "message": detail}),
    Err(ParseError::Helper(message)) => json!({"status": "helper_error", "message": message}),
  }
}
