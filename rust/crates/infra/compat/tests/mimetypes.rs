//! The built-in content-type table against CPython's `mimetypes.guess_type` for every built-in suffix and
//! for the paths that combine them (`rust/fixtures/golden/mimetypes_golden.json`).

use charliebot_compat::mimetypes::MimeTypes;
use serde_json::Value;

const FIXTURE: &str = include_str!("../../../../fixtures/golden/mimetypes_golden.json");

#[test]
fn guesses_match_recorded() {
  let fixture: Value = serde_json::from_str(FIXTURE).expect("the fixture is JSON");
  let cases = fixture["cases"].as_array().expect("the fixture lists cases");
  assert!(cases.len() > 200, "the fixture covers every built-in suffix");
  let database = MimeTypes::builtin();
  for case in cases {
    let path = case["path"].as_str().expect("a case has a path");
    assert_eq!(database.guess_type(path), case["type"].as_str(), "guess_type({path:?})");
  }
}
