//! The shared template environment's `tojson` and `urlencode`, and its printing, against the output
//! Jinja2 gives for the same templates and contexts (`rust/fixtures/golden/jinja_filters_golden.json`).

use charliebot_compat::jinja;
use minijinja::Value;
use serde_json::Value as Json;

const FIXTURE: &str = include_str!("../../../../fixtures/golden/jinja_filters_golden.json");

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

/// A page renders a value as text in the handler and prints it with `|safe`; those bytes are the filter's.
#[test]
fn tojson_text_matches_the_filter() {
  let value = serde_json::json!({
    "text": "<a href=\"x\">&'</a> ünïcode \u{7f}",
    "sorted": {"z": 1, "a": 2, "m": [null, true, 2.5]},
    "empty": [],
  });
  let environment = jinja::environment(|_| Ok(None));
  let rendered = environment
    .render_str("{{ value|tojson }}", Value::from_serialize(serde_json::json!({"value": value})))
    .expect("the tojson filter renders");
  assert_eq!(rendered, jinja::tojson_text(&value));
}
