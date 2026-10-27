use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use charliebot_store::trigger_files;
use charliebot_triggers::triggers::PendingTrigger;
use serde_json::Value;

fn repo_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(4).expect("crate is nested under rust/crates").to_path_buf()
}

fn run_python(action: &str, directory: &Path) -> Vec<Value> {
  let root = repo_root();
  let script = root.join("rust/crates/runtime/triggers/tests/trigger_file_roundtrip.py");
  let output = Command::new("uv")
    .args(["run", "python"])
    .arg(script)
    .arg(action)
    .arg(directory)
    .current_dir(&root)
    .output()
    .expect("uv launches the trigger-file compatibility script");
  assert!(output.status.success(), "python helper failed: {}", String::from_utf8_lossy(&output.stderr));
  serde_json::from_slice(&output.stdout).expect("python emits JSON records")
}

#[test]
// Python: tests/test_trigger_pid_watch.py::test_trigger_file_without_exit_fields_still_loads
// Python: all TriggerStatus and WatchTarget records in src/infra/models.py
fn python_and_rust_read_and_write_equal_trigger_records() {
  let root = tempfile::tempdir().expect("temporary compatibility root");
  let python_dir = root.path().join("python");
  let rust_dir = root.path().join("rust");
  fs::create_dir_all(&rust_dir).expect("Rust record directory");

  let python_records = run_python("write", &python_dir);
  let mut rust_records = trigger_files::iter_trigger_file_stats(&python_dir).expect("walk Python trigger files");
  rust_records.sort_by(|left, right| left.0.cmp(&right.0));
  let loaded = rust_records
    .iter()
    .map(|(path, _)| {
      let body = fs::read(path).expect("read Python trigger file");
      let trigger: PendingTrigger = serde_json::from_slice(&body).expect("Rust reads Python trigger record");
      serde_json::to_value(trigger).expect("serialize Rust trigger record")
    })
    .collect::<Vec<_>>();
  assert_eq!(python_records, loaded, "Python records must survive Rust deserialization and serialization");

  for record in &loaded {
    let trigger: PendingTrigger = serde_json::from_value(record.clone()).expect("Rust record deserializes");
    let path = rust_dir.join(format!("{}.json", trigger.id));
    let mut body = serde_json::to_vec_pretty(&trigger).expect("serialize Rust trigger file");
    body.push(b'\n');
    fs::write(path, body).expect("write Rust trigger file");
  }
  let from_rust = run_python("read", &rust_dir);
  assert_eq!(loaded, from_rust, "Python must read Rust trigger records without changing their fields");

  assert_eq!(loaded.len(), 7);
  assert!(loaded.iter().any(|record| record["status"] == "pending"));
  assert!(loaded.iter().any(|record| record["status"] == "fired"));
  assert!(loaded.iter().any(|record| record["status"] == "cancelled"));
  assert!(loaded.iter().any(|record| record["watch_targets"][0]["kind"] == "local_pid"));
  assert!(loaded.iter().any(|record| record["watch_targets"][0]["kind"] == "remote_pid"));
  assert!(loaded.iter().any(|record| record["watch_targets"][0]["kind"] == "slurm_job"));
  assert!(
    loaded
      .iter()
      .any(|record| record["watch_targets"][0]["kind"] == "slurm_job" && record["watch_targets"][0]["host"].is_null())
  );
}
