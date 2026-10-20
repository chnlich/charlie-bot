use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use charliebot_core::yaml;
use serde_json::{Value, json};

/// Several paragraphs. Some lines pass column 80, so the emitter folds them; one line has an apostrophe.
const LONG_PROMPT: &str = "Review the open pull requests in the release branch and summarize what changed since \
  the last cut.\n\nFor each pull request, name its author, the files it touches, and whether its tests pass. \
  Flag any change that edits a migration, because a migration cannot be rolled back.\nEnd with one line that \
  says whether the branch is ready to cut, and don't merge anything else.";

fn repo_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3).expect("crate is nested under rust/crates").to_path_buf()
}

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
  corpus
}

/// Runs the PyYAML helper on each case: the value and the Rust dump of it. It returns PyYAML's dump of the value and
/// whether the Rust dump reads back as the value.
fn python_check(corpus: &[Value], rust: &[String]) -> Vec<Value> {
  let cases: Vec<Value> = corpus.iter().zip(rust).map(|(value, rust)| json!({"value": value, "rust": rust})).collect();
  let root = repo_root();
  let script = root.join("rust/crates/charliebot-core/tests/yaml_dump_parity.py");
  let mut child = Command::new("uv")
    .args(["run", "python"])
    .arg(script)
    .current_dir(root)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("uv starts the PyYAML compatibility script");
  child.stdin.take().expect("Python stdin is piped").write_all(&serde_json::to_vec(&cases).unwrap()).unwrap();
  let output = child.wait_with_output().expect("PyYAML compatibility script exits");
  assert!(output.status.success(), "PyYAML helper failed: {}", String::from_utf8_lossy(&output.stderr));
  serde_json::from_slice(&output.stdout).expect("the PyYAML helper prints a JSON list")
}

#[test]
fn core_yaml_writer_matches_pyyaml_safe_c_dumper_for_the_corpus() {
  let corpus = corpus();
  let rust: Vec<String> = corpus.iter().map(|value| yaml::dump(value).unwrap()).collect();
  let checks = python_check(&corpus, &rust);
  assert_eq!(checks.len(), corpus.len());
  for ((value, rust), check) in corpus.iter().zip(&rust).zip(&checks) {
    assert_eq!(rust, check["pyyaml"].as_str().expect("PyYAML's dump is a string"), "PyYAML bytes differ for {value}");
    assert_eq!(check["reads_back"], json!(true), "yaml.safe_load of the Rust dump is not {value}");
  }
}
