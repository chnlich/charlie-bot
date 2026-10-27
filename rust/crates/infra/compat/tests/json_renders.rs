//! The JSON renders against the bytes orjson and Starlette write for the same values
//! (`rust/fixtures/python/json_renders.json`, written by `rust/scripts/gen_compat_fixtures.py`).

use charliebot_compat::json::{fast_json_bytes, starlette_bytes};
use serde_json::Value;

const FIXTURE: &str = include_str!("../../../../fixtures/python/json_renders.json");

#[test]
fn renders_match_python_byte_for_byte() {
  let fixture: Value = serde_json::from_str(FIXTURE).expect("the fixture is JSON");
  let cases = fixture["cases"].as_array().expect("the fixture lists cases");
  assert!(cases.len() > 500, "the fixture covers the float range and the string escapes");
  for case in cases {
    let value = &case["value"];
    let orjson = String::from_utf8(fast_json_bytes(value)).expect("orjson writes UTF-8");
    assert_eq!(orjson, case["orjson"], "orjson of {value}");
    let starlette = String::from_utf8(starlette_bytes(value)).expect("Starlette writes UTF-8");
    assert_eq!(starlette, case["starlette"], "Starlette JSON of {value}");
  }
}
