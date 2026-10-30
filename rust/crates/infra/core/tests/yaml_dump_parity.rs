use charliebot_core::yaml;
use serde_json::{Value, json};

const PYTHON_FIXTURE: &str = include_str!("fixtures/yaml_dump_parity_golden.json");

/// Several paragraphs. Some lines pass column 80, so the emitter folds them; one line has an apostrophe.
const LONG_PROMPT: &str = "Review the open pull requests in the release branch and summarize what changed since \
  the last cut.\n\nFor each pull request, name its author, the files it touches, and whether its tests pass. \
  Flag any change that edits a migration, because a migration cannot be rolled back.\nEnd with one line that \
  says whether the branch is ready to cut, and don't merge anything else.";

/// Each case is dumped on its own, so a mismatch names its value. The values of `cron_yaml_cases.md` sit under the key
/// `k`, the shape that file reads them in.
fn corpus() -> Vec<Value> {
  let mut corpus = vec![json!({
    "name": "daily",
    "cron": "0 8 * * *",
    "enabled": true,
    "project": null,
    "blank": "",
    "toggle": "on",
    "yes": "yes",
    "colon": "a: b",
    "comment": "x # y",
    "leading": " leading",
    "trailing": "trailing ",
    "date": "2026-10-20",
    "unicode": "雪と café",
    "multiline": "line one\n\nline three",
    "items": ["simple", 1, true, null, {"nested": "value"}],
  })];
  for value in ["1_000", "0x1F", "0b101", "12:30", "2026-10-20 08:00:00", "=", "<<", "1e3", "inf", "017", "+1", "y"] {
    corpus.push(json!({"k": value}));
  }
  for value in ["-foo", "?foo", ":foo", "- foo", "? foo", "-"] {
    corpus.push(json!({"k": value}));
  }
  for value in [json!(0.5), json!(1e20), json!(1e-7), json!(-0.0), json!(100.0)] {
    corpus.push(json!({"k": value}));
  }
  let folded = vec!["word"; 20].join(" ");
  corpus.push(json!({"prompt": folded}));
  corpus.push(json!({
    "items": [folded, {"name": "x", "prompt": folded}, {"a": {"b": 1}}, {"c": ["x", "y"]}, [["a", "b"], []]],
    "nested": {"prompt": folded},
  }));
  corpus.push(json!({"prompt": "a\n  b"}));
  corpus.push(json!({"prompt": "a \nb"}));
  corpus.push(json!({"prompt": "tab\there"}));
  corpus.push(json!({"prompt": "it's\nfine"}));
  corpus.push(json!({"prompt": "ends with a newline\n"}));
  corpus.push(json!({"prompt": LONG_PROMPT}));
  corpus.push(json!({"prompt": format!("{}prompt\t{}", "long ".repeat(36), "tail ".repeat(8))}));
  corpus.push(json!({"prompt": format!("{} \n{}", "a".repeat(90), "second line ".repeat(8))}));
  corpus.push(json!({
    "prompt": "\0\u{7}\u{8}\t\n\u{b}\u{c}\r\u{1b}\u{1}\u{7f}\u{85}\u{a0}\u{2028}\u{2029}\u{feff}\"\\"
  }));
  for value in ["\u{1}", "\u{1b}", "\u{7f}", "\u{85}", "\u{2028}", "\u{feff}"] {
    corpus.push(json!({"prompt": value}));
  }
  corpus.push(json!({"key\tneeds quotes": "value"}));
  corpus.push(json!({"prompt": "Ship it \u{1f680}"}));
  corpus.push(json!({"prompt": "caf\u{e9} \u{1f600}\ttab"}));
  corpus.push(json!({"prompt": format!("{}  \ttail", "a".repeat(90))}));
  corpus
}

#[test]
fn core_yaml_writer_matches_pyyaml_safe_c_dumper_for_the_corpus() {
  let corpus = corpus();
  let rust: Vec<String> = corpus.iter().map(|value| yaml::dump(value).unwrap()).collect();
  let fixture: Value = serde_json::from_str(PYTHON_FIXTURE).unwrap();
  assert_eq!(fixture["base_commit"], "d4021ce3993d7e36b2709f00caf578e1f8541407");
  let cases = fixture["cases"].as_array().unwrap();
  assert_eq!(cases.len(), corpus.len());
  for ((value, rust), case) in corpus.iter().zip(&rust).zip(cases) {
    assert_eq!(case["request"]["value"], *value);
    assert_eq!(case["request"]["rust"], *rust);
    assert_eq!(
      rust,
      case["expected"]["pyyaml"].as_str().expect("PyYAML's dump is a string"),
      "PyYAML bytes differ for {value}"
    );
    assert_eq!(case["expected"]["reads_back"], json!(true), "yaml.safe_load of the Rust dump is not {value}");
  }
}
