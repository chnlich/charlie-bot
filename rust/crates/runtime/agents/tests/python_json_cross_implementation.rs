//! `charliebot_compat::python_json` against recorded `json.loads` outputs over one deterministic corpus.
//!
//! The fixture records what `python_json_reference.py` returned at its named commit. The port must give the same outcome
//! class and, for an error, the same `str(ValueError)` text, and for a value the same `json.dumps` rendering. The corpus
//! must reach every message the JSON decoder and scanner raise.

use charliebot_compat::python_json::{self, LoadError};
use charliebot_core::pyfmt;
use serde_json::{Value, json};

const PYTHON_FIXTURE: &str = include_str!("fixtures/python_json_reference.json");

/// The messages `json.loads` raises, with the C scanner's wording (`_json.c`). The decoder's two `repr` forms, `Invalid
/// \escape: <repr>` and `Invalid control character <repr> at`, are raised only by `json/decoder.py`'s pure-Python
/// scanner, which `json.loads` does not use, so no input reaches them through it.
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

/// The `UnicodeDecodeError` reasons a body that is not UTF-8 (or not its detected wide codec) can give.
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

#[test]
fn python_json_matches_json_loads_over_the_corpus() {
  let records = recorded_reference();
  assert!(records.len() > 9000, "the corpus holds {} inputs", records.len());

  let mut mismatches = Vec::new();
  let mut python_errors = Vec::new();
  for record in &records {
    let mode = record["mode"].as_str().unwrap();
    let input = record["input"].as_str().unwrap();
    let expected = match record["outcome"].as_str().unwrap() {
      "value" => Outcome::Value(record["text"].as_str().unwrap().to_string()),
      "error" => {
        let text = record["text"].as_str().unwrap().to_string();
        python_errors.push(text.clone());
        Outcome::Error(text)
      }
      "unrepresentable" => Outcome::Unrepresentable,
      other => panic!("unknown outcome {other}"),
    };
    let actual = match python_json_load(mode, input) {
      Ok(value) => Outcome::Value(pyfmt::json_dumps(&value, &pyfmt::STARLETTE_JSON)),
      Err(LoadError::Value(text)) => Outcome::Error(text),
      Err(LoadError::Unrepresentable(_)) => Outcome::Unrepresentable,
    };
    if actual != expected {
      mismatches.push(format!("{mode} {input:?}: python {expected:?}, rust {actual:?}"));
    }
  }
  assert!(
    mismatches.is_empty(),
    "{} of {} inputs differ:\n{}",
    mismatches.len(),
    records.len(),
    mismatches[..mismatches.len().min(20)].join("\n")
  );

  for message in REQUIRED_MESSAGES {
    assert!(
      python_errors.iter().any(|text| text.starts_with(message) || text.contains(&format!(": {message}"))),
      "no corpus input reaches {message:?}"
    );
  }
  for reason in REQUIRED_CODEC_REASONS {
    assert!(
      python_errors.iter().any(|text| text.contains(reason)),
      "no corpus input reaches the codec reason {reason:?}"
    );
  }
}

fn python_json_load(mode: &str, input: &str) -> Result<Value, LoadError> {
  match mode {
    "bytes" => python_json::loads(&hex_bytes(input)),
    "text" => python_json::loads_text(input),
    other => panic!("unknown corpus mode {other}"),
  }
}

fn hex_bytes(hex: &str) -> Vec<u8> {
  (0..hex.len()).step_by(2).map(|at| u8::from_str_radix(&hex[at..at + 2], 16).unwrap()).collect()
}

/// The outputs `json.loads` recorded for the deterministic input corpus.
fn recorded_reference() -> Vec<Value> {
  let fixture: Value = serde_json::from_str(PYTHON_FIXTURE).unwrap();
  assert_eq!(fixture["source"], "rust/crates/runtime/agents/tests/python_json_reference.py");
  assert_eq!(fixture["base_commit"], "d4021ce3993d7e36b2709f00caf578e1f8541407");
  fixture["cases"]
    .as_array()
    .unwrap()
    .iter()
    .map(|case| {
      json!({
        "mode": case["request"]["mode"],
        "input": case["request"]["input"],
        "outcome": case["expected"]["outcome"],
        "text": case["expected"]["text"],
      })
    })
    .collect()
}
