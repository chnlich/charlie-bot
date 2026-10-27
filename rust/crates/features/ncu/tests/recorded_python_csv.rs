use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

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

#[derive(Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum Run {
  Completed { success: bool, stdout_file: String, stderr: String },
  NotFound,
  Timeout,
}

#[test]
fn recorded_python_csv_results_match_every_canned_case() {
  let tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
  let fixture: Value =
    serde_json::from_slice(&fs::read(tests.join("fixtures/python_csv_results.json")).unwrap()).unwrap();
  assert_eq!(fixture["source"]["helper"], "rust/crates/features/ncu/tests/csv_differential.py");
  assert_eq!(fixture["source"]["base_commit"], "a572e7ca71858d9ba52f0abc2726fa44213210ac");

  let manifest: Manifest = serde_json::from_slice(&fs::read(tests.join("canned/cases.json")).unwrap()).unwrap();
  let expected = fixture["cases"].as_object().unwrap();
  let actual =
    manifest.cases.into_iter().map(|case| (case.name.clone(), parse_case(case, &tests))).collect::<Map<_, _>>();
  assert_eq!(actual, *expected, "all fields in the recorded Python results are compared");

  let mut planted = expected.clone();
  planted["two_kernels"]["report"]["kernels"][0]["name"] = json!("synthetic_gemm (planted)");
  let differences = actual
    .keys()
    .chain(planted.keys())
    .collect::<BTreeSet<_>>()
    .into_iter()
    .filter(|name| actual.get(*name) != planted.get(*name))
    .count();
  assert_eq!(differences, 1, "a changed Python result is isolated to one case");
}

fn parse_case(case: Case, tests: &Path) -> Value {
  let outcome = match case.run {
    Run::Completed { success, stdout_file, stderr } => Outcome::Completed {
      success,
      stdout: fs::read_to_string(tests.join("canned").join(stdout_file)).unwrap(),
      stderr,
    },
    Run::NotFound => Outcome::NotFound,
    Run::Timeout => Outcome::TimedOut,
  };
  match csv_import::parse(outcome) {
    Ok(report) => json!({"status": "ok", "report": report}),
    Err(ParseError::Report(message)) => json!({"status": "report_error", "message": message}),
    Err(ParseError::Internal { kind, detail }) => json!({"status": "internal_error", "kind": kind, "message": detail}),
    Err(ParseError::Helper(message)) => json!({"status": "helper_error", "message": message}),
  }
}
