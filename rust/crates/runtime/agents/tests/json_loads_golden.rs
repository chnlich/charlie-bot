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
/// \escape: <repr>` and `Invalid control character <repr> at`, are raised only by the pure scanner. The decode path
/// does not use that scanner, so no input reaches them.
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
const SAMPLE_SHARDS: usize = 4;

#[derive(Deserialize)]
struct RecordedFixture<'a> {
  base_commit: &'a str,
  source_case_count: usize,
  #[serde(borrow)]
  samples: Vec<RecordedCase<'a>>,
  #[serde(borrow)]
  required_error_cases: Vec<RecordedCase<'a>>,
}

#[derive(Deserialize)]
struct RecordedCase<'a> {
  #[serde(borrow)]
  request: &'a RawValue,
  #[serde(borrow)]
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

macro_rules! sample_shard {
  ($name:ident, $index:expr) => {
    #[test]
    fn $name() {
      compare_sample_shard($index);
    }
  };
}

sample_shard!(json_loads_matches_recorded_sample_shard_0, 0);
sample_shard!(json_loads_matches_recorded_sample_shard_1, 1);
sample_shard!(json_loads_matches_recorded_sample_shard_2, 2);
sample_shard!(json_loads_matches_recorded_sample_shard_3, 3);

fn compare_sample_shard(shard: usize) {
  let fixture: RecordedFixture = serde_json::from_str(GOLDEN_FIXTURE).unwrap();
  assert_eq!(fixture.base_commit, "d4021ce3993d7e36b2709f00caf578e1f8541407");
  assert!(fixture.source_case_count > 9000, "the corpus holds {} inputs", fixture.source_case_count);
  assert_eq!(fixture.samples.len(), fixture.source_case_count.div_ceil(SAMPLE_STEP));

  let mut mismatches = Vec::new();
  let mut sampled = 0;
  let samples = fixture.samples.iter().enumerate().filter(|(index, _)| index % SAMPLE_SHARDS == shard).map(|(_, r)| r);
  let required = fixture.required_error_cases.iter().filter(|_| shard == 0);
  for record in samples.chain(required) {
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

  let mut found_messages = [false; REQUIRED_MESSAGES.len()];
  let mut found_codec_reasons = [false; REQUIRED_CODEC_REASONS.len()];
  for record in &fixture.required_error_cases {
    assert_eq!(record.expected.outcome, "error");
    let text = record.expected.text.as_deref().unwrap();
    for (index, message) in REQUIRED_MESSAGES.iter().enumerate() {
      found_messages[index] |= text.starts_with(message)
        || text.match_indices(": ").any(|(offset, _)| text[offset + 2..].starts_with(message));
    }
    for (index, reason) in REQUIRED_CODEC_REASONS.iter().enumerate() {
      found_codec_reasons[index] |= text.contains(reason);
    }
  }
  for (message, found) in REQUIRED_MESSAGES.iter().zip(found_messages) {
    assert!(found, "no corpus input reaches {message:?}");
  }
  for (reason, found) in REQUIRED_CODEC_REASONS.iter().zip(found_codec_reasons) {
    assert!(found, "no corpus input reaches the codec reason {reason:?}");
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
