//! `charliebot_core::json_loads` against recorded decoder outputs over one deterministic corpus.
//!
//! The fixture records the outcome the reference decoder gave when it was recorded. The loader must give the same
//! outcome class and, for an error, the same message text, and for a value the same JSON rendering. The corpus must
//! reach every message the JSON decoder and scanner raise.

use std::borrow::Cow;

use charliebot_core::json_loads::{self, LoadError};
use charliebot_core::text_repr;
use serde::Deserialize;
use serde_json::{Value, value::RawValue};

const GOLDEN_FIXTURE: &str = include_str!("fixtures/json_loads_golden.json");

/// The messages a JSON decode raises, in the C scanner's wording. The decoder's two `repr` forms, `Invalid
/// \escape: <repr>` and `Invalid control character <repr> at`, are raised only by the pure scanner, which the
/// decode path does not use, so no input reaches them.
const REQUIRED_MESSAGES: [&str; 12] = [
  "Expecting value",
  "Expecting property name enclosed in double quotes",
  "Expecting ':' delimiter",
  "Expecting ',' delimiter",
  "Illegal trailing comma before end of object",
  "Illegal trailing comma before end of array",
  "Unterminated string starting at",
  "Invalid control character at",
  r"Invalid \escape",
  r"Invalid \uXXXX escape",
  "Extra data",
  "Unexpected UTF-8 BOM (decode using utf-8-sig)",
];

/// The decode-error reasons a body that is not UTF-8 (or not its detected wide codec) can give.
const REQUIRED_CODEC_REASONS: [&str; 5] = [
  "invalid start byte",
  "invalid continuation byte",
  "unexpected end of data",
  "truncated data",
  "code point not in range(0x110000)",
];

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
  Value(String),
  Error(String),
  Unrepresentable,
}

/// One input in every this many is decoded and compared.
const SAMPLE_STEP: usize = 128;

#[derive(Deserialize)]
struct RecordedFixture<'a> {
  base_commit: &'a str,
  cases: &'a RawValue,
}

#[derive(Deserialize)]
struct RecordedCase<'a> {
  #[serde(borrow)]
  request: &'a RawValue,
  expected: RecordedOutcome<'a>,
}

#[derive(Deserialize)]
struct RecordedRequest {
  mode: String,
  input: String,
}

#[derive(Deserialize)]
struct RecordedOutcome<'a> {
  #[serde(borrow)]
  outcome: Cow<'a, str>,
  #[serde(borrow)]
  text: Option<Cow<'a, str>>,
}

#[test]
fn json_loads_matches_recorded_over_a_sample_of_the_corpus() {
  let fixture: RecordedFixture = serde_json::from_str(GOLDEN_FIXTURE).unwrap();
  assert_eq!(fixture.base_commit, "d4021ce3993d7e36b2709f00caf578e1f8541407");
  let records: Vec<RecordedCase> = serde_json::from_str(fixture.cases.get()).unwrap();
  assert!(records.len() > 9000, "the corpus holds {} inputs", records.len());

  let mut mismatches = Vec::new();
  let mut sampled = 0;
  for record in records.iter().step_by(SAMPLE_STEP) {
    sampled += 1;
    let request: RecordedRequest = serde_json::from_str(record.request.get()).unwrap();
    let mode = request.mode.as_str();
    let input = request.input.as_str();
    let expected = match record.expected.outcome.as_ref() {
      "value" => Outcome::Value(record.expected.text.as_deref().unwrap().to_string()),
      "error" => Outcome::Error(record.expected.text.as_deref().unwrap().to_string()),
      "unrepresentable" => Outcome::Unrepresentable,
      other => panic!("unknown outcome {other}"),
    };
    let actual = match load_by_mode(mode, input) {
      Ok(value) => Outcome::Value(text_repr::json_dumps(&value, &text_repr::STARLETTE_JSON)),
      Err(LoadError::Value(text)) => Outcome::Error(text),
      Err(LoadError::Unrepresentable(_)) => Outcome::Unrepresentable,
    };
    if actual != expected {
      mismatches.push(format!("{mode} {input:?}: python {expected:?}, rust {actual:?}"));
    }
  }
  assert!(
    mismatches.is_empty(),
    "{} of {} sampled inputs differ:\n{}",
    mismatches.len(),
    sampled,
    mismatches[..mismatches.len().min(20)].join("\n")
  );

  let python_errors: Vec<&str> = records
    .iter()
    .filter(|record| record.expected.outcome.as_ref() == "error")
    .filter_map(|record| record.expected.text.as_deref())
    .collect();
  for message in REQUIRED_MESSAGES {
    assert!(
      python_errors.iter().any(|text| text.starts_with(message) || text.contains(&format!(": {message}"))),
      "no corpus input reaches {message:?}"
    );
  }
  for reason in REQUIRED_CODEC_REASONS {
    assert!(python_errors.iter().any(|text| text.contains(reason)), "no corpus input reaches the codec reason {reason:?}");
  }
}

fn load_by_mode(mode: &str, input: &str) -> Result<Value, LoadError> {
  match mode {
    "bytes" => json_loads::loads(&hex_bytes(input)),
    "text" => json_loads::loads_text(input),
    other => panic!("unknown corpus mode {other}"),
  }
}

fn hex_bytes(hex: &str) -> Vec<u8> {
  (0..hex.len()).step_by(2).map(|at| u8::from_str_radix(&hex[at..at + 2], 16).unwrap()).collect()
}
