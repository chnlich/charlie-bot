//! The shared template environment's `tojson` and `urlencode`, and its printing, against the output
//! Jinja2 gives for the same templates and contexts (`rust/fixtures/python/jinja_filters.json`).

use charliebot_compat::jinja;
use minijinja::Value;
use serde_json::Value as Json;

const FIXTURE: &str = include_str!("../../../../fixtures/python/jinja_filters.json");

#[test]
fn filters_and_printing_match_jinja2() {
  let fixture: Json = serde_json::from_str(FIXTURE).expect("the fixture is JSON");
  let cases = fixture["cases"].as_array().expect("the fixture lists cases");
  assert!(cases.len() > 25, "the fixture covers both filters' value kinds");
  let environment = jinja::environment(|_| Ok(None));
  for case in cases {
    let template = case["template"].as_str().expect("a case has a template");
    let output = environment
      .render_str(template, Value::from_serialize(&case["context"]))
      .unwrap_or_else(|error| panic!("{template}: {error}"));
    assert_eq!(output, case["output"].as_str().expect("a case has its output"), "{template} with {}", case["context"]);
  }
}
