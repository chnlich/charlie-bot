//! The `status-reread` check answers each JSON sample of a turn: the verdict and the reason's named parts.

use charliebot_tasks::turn_hooks::{CheckResult, TurnInput};
use charliebot_web_reply::audit::check_status_reread;
use serde::Deserialize;

#[derive(Deserialize)]
struct Sample {
  name: String,
  input: TurnInput,
  expect: String,
  #[serde(default)]
  reason_contains: Vec<String>,
}

#[test]
fn each_sample_turn_gets_its_verdict_and_reason() {
  let samples: Vec<Sample> =
    serde_json::from_str(include_str!("fixtures/status_reread_samples.json")).expect("the samples parse");
  assert!(!samples.is_empty());
  for sample in samples {
    let answer = check_status_reread(&sample.input);
    let (verdict, text) = match &answer {
      CheckResult::Pass => ("pass", ""),
      CheckResult::Block(text) => ("block", text.as_str()),
      CheckResult::Notice(text) => ("notice", text.as_str()),
    };
    assert_eq!(verdict, sample.expect, "{}: {answer:?}", sample.name);
    for fragment in &sample.reason_contains {
      assert!(text.contains(fragment.as_str()), "{}: the reason lacks {fragment:?}: {text}", sample.name);
    }
  }
}
