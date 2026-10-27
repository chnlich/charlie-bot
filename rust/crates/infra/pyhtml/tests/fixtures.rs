use std::collections::HashMap;
use std::io::Read;

use charliebot_pyhtml::{Event, Opcode, OpcodeTag, escape, opcodes, parse, unescape};
use flate2::read::GzDecoder;
use serde::Deserialize;

#[derive(Deserialize)]
struct Corpus {
  cpython_version: String,
  documents: Vec<CorpusDocument>,
  opcodes: Vec<OpcodeInput>,
}

#[derive(Deserialize)]
struct FixtureProvenance {
  source: String,
  base_commit: String,
  fixtures: Vec<String>,
}

#[derive(Deserialize)]
struct CorpusDocument {
  id: String,
  source: String,
}

#[derive(Deserialize)]
struct OpcodeInput {
  id: String,
  a: Vec<String>,
  b: Vec<String>,
}

#[derive(Deserialize)]
struct Expected {
  cpython_version: String,
  documents: Vec<ExpectedDocument>,
  unescape: Vec<StringCase>,
  escape: Vec<EscapeCase>,
  opcodes: Vec<ExpectedOpcodes>,
}

#[derive(Deserialize)]
struct ExpectedDocument {
  id: String,
  convert_charrefs_false: Vec<Event>,
  convert_charrefs_true: Vec<Event>,
}

#[derive(Deserialize)]
struct StringCase {
  input: String,
  expected: String,
}

#[derive(Deserialize)]
struct EscapeCase {
  input: String,
  quote: bool,
  expected: String,
}

#[derive(Deserialize)]
struct ExpectedOpcodes {
  id: String,
  expected: Vec<ExpectedOpcode>,
}

#[derive(Deserialize)]
struct ExpectedOpcode {
  tag: String,
  a_start: usize,
  a_end: usize,
  b_start: usize,
  b_end: usize,
}

fn fixtures() -> (Corpus, Expected) {
  let provenance: FixtureProvenance =
    serde_json::from_str(include_str!("fixtures/source.json")).expect("fixture provenance is valid");
  assert_eq!(provenance.source, "rust/crates/infra/pyhtml/tests/fixtures/generate_fixtures.py");
  assert_eq!(provenance.base_commit, "a572e7ca71858d9ba52f0abc2726fa44213210ac");
  assert_eq!(
    provenance.fixtures,
    vec![
      "corpus.json.gz".to_string(),
      "expected.json.gz".to_string(),
      "rust/crates/infra/pyhtml/src/html5_entities.json".to_string(),
    ]
  );
  (
    read_gzip_json(include_bytes!("fixtures/corpus.json.gz")),
    read_gzip_json(include_bytes!("fixtures/expected.json.gz")),
  )
}

fn read_gzip_json<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> T {
  let mut decoder = GzDecoder::new(bytes);
  let mut json = String::new();
  decoder.read_to_string(&mut json).expect("fixture JSON decompresses");
  serde_json::from_str(&json).expect("valid fixture JSON")
}

#[test]
fn tokenizer_matches_cpython_fixture_events() {
  let (corpus, expected) = fixtures();
  assert_eq!(corpus.cpython_version, "3.14.7");
  assert_eq!(expected.cpython_version, corpus.cpython_version);
  assert_eq!(corpus.documents.len(), expected.documents.len());
  let expected_by_id: HashMap<&str, &ExpectedDocument> =
    expected.documents.iter().map(|document| (document.id.as_str(), document)).collect();

  for document in &corpus.documents {
    let fixture = expected_by_id[document.id.as_str()];
    assert_eq!(
      parse(&document.source, false).expect("tokenizer parse succeeds"),
      fixture.convert_charrefs_false,
      "{} false",
      document.id
    );
    assert_eq!(
      parse(&document.source, true).expect("tokenizer parse succeeds"),
      fixture.convert_charrefs_true,
      "{} true",
      document.id
    );
  }
}

#[test]
fn entity_and_escape_helpers_match_cpython_fixtures() {
  let (_, expected) = fixtures();
  for case in expected.unescape {
    assert_eq!(unescape(&case.input), case.expected, "unescape input {:?}", case.input);
  }
  for case in expected.escape {
    assert_eq!(escape(&case.input, case.quote), case.expected, "escape input {:?}, quote={}", case.input, case.quote);
  }
}

#[test]
fn opcodes_match_cpython_sequence_matcher_fixtures() {
  let (corpus, expected) = fixtures();
  let expected_by_id: HashMap<&str, &ExpectedOpcodes> =
    expected.opcodes.iter().map(|fixture| (fixture.id.as_str(), fixture)).collect();
  assert_eq!(corpus.opcodes.len(), expected.opcodes.len());
  for input in &corpus.opcodes {
    let fixture = expected_by_id[input.id.as_str()];
    let actual = opcodes(&input.a, &input.b);
    let converted: Vec<(&str, usize, usize, usize, usize)> = actual.iter().map(opcode_tuple).collect();
    let expected_tuples: Vec<(&str, usize, usize, usize, usize)> =
      fixture.expected.iter().map(expected_opcode_tuple).collect();
    assert_eq!(converted, expected_tuples, "{}", input.id);
  }
}

fn opcode_tuple(opcode: &Opcode) -> (&'static str, usize, usize, usize, usize) {
  let tag = match opcode.tag {
    OpcodeTag::Replace => "replace",
    OpcodeTag::Delete => "delete",
    OpcodeTag::Insert => "insert",
    OpcodeTag::Equal => "equal",
  };
  (tag, opcode.a_start, opcode.a_end, opcode.b_start, opcode.b_end)
}

fn expected_opcode_tuple(opcode: &ExpectedOpcode) -> (&str, usize, usize, usize, usize) {
  (opcode.tag.as_str(), opcode.a_start, opcode.a_end, opcode.b_start, opcode.b_end)
}
