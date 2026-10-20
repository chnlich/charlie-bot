use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use charliebot_core::yaml;
use serde_json::{Value, json};

fn repo_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3).expect("crate is nested under rust/crates").to_path_buf()
}

fn python_dump(value: &Value) -> String {
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
  child.stdin.take().expect("Python stdin is piped").write_all(&serde_json::to_vec(value).unwrap()).unwrap();
  let output = child.wait_with_output().expect("PyYAML compatibility script exits");
  assert!(output.status.success(), "PyYAML helper failed: {}", String::from_utf8_lossy(&output.stderr));
  String::from_utf8(output.stdout).expect("PyYAML emits UTF-8")
}

#[test]
fn core_yaml_writer_matches_pyyaml_safe_c_dumper_for_cron_values() {
  let value = json!({
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
  });
  assert_eq!(yaml::dump(&value).unwrap(), python_dump(&value));
}
