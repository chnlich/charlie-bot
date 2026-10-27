use std::fs;
use std::path::Path;

use charliebot_store::trigger_files;
use charliebot_triggers::triggers::PendingTrigger;
use serde_json::Value;

const PYTHON_FIXTURE: &str = include_str!("fixtures/trigger_file_roundtrip.json");

#[test]
// Python: tests/test_trigger_pid_watch.py::test_trigger_file_without_exit_fields_still_loads
// Python: all TriggerStatus and WatchTarget records in src/infra/models.py
fn python_and_rust_read_and_write_equal_trigger_records() {
  let fixture: Value = serde_json::from_str(PYTHON_FIXTURE).unwrap();
  assert_eq!(fixture["source"], "rust/crates/runtime/triggers/tests/trigger_file_roundtrip.py");
  assert_eq!(fixture["base_commit"], "a572e7ca71858d9ba52f0abc2726fa44213210ac");
  let mut python_records = fixture["expected"].as_array().unwrap().clone();
  materialize_fixture_values(&mut python_records);
  let root = tempfile::tempdir().expect("temporary compatibility root");
  let python_dir = root.path().join("python");
  let rust_dir = root.path().join("rust");
  fs::create_dir_all(&python_dir).expect("recorded Python trigger directory");
  fs::create_dir_all(&rust_dir).expect("Rust record directory");
  write_recorded_files(&python_dir, &fixture["python_written_files"]);

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
  // The recorded Python read below holds for exactly these bytes, so Rust must still write them.
  for (name, text) in fixture["rust_written_files"].as_object().unwrap() {
    let written = fs::read_to_string(rust_dir.join(name)).expect("read Rust trigger file");
    assert_eq!(written, text.as_str().unwrap(), "Rust trigger file {name} differs from the recorded bytes");
  }
  let mut from_rust = trigger_files::iter_trigger_file_stats(&rust_dir)
    .expect("walk Rust trigger files")
    .into_iter()
    .map(|(path, _)| {
      let body = fs::read(path).expect("read Rust trigger file");
      let trigger: PendingTrigger = serde_json::from_slice(&body).expect("read Rust trigger record");
      serde_json::to_value(trigger).expect("serialize Rust trigger record")
    })
    .collect::<Vec<_>>();
  from_rust.sort_by(|left, right| left["id"].as_str().cmp(&right["id"].as_str()));
  let python_read = fixture["python_read_of_rust_files"].as_array().unwrap();
  assert_eq!(python_read, &from_rust, "Python must read Rust trigger records without changing their fields");

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

fn write_recorded_files(directory: &Path, files: &Value) {
  for (name, text) in files.as_object().unwrap() {
    fs::write(directory.join(name), text.as_str().unwrap()).expect("write recorded Python trigger file");
  }
}

fn materialize_fixture_values(records: &mut [Value]) {
  fn materialize(value: &mut Value) {
    match value {
      Value::String(text) => {
        *text = text
          .replace("$TIME", "2026-01-02T03:04:05.123456Z")
          .replace("$HOST", "synthetic-host")
          .replace("$EXIT_FILE", "/synthetic/exit")
          .replace("$LOG_FILE", "/synthetic/log");
      }
      Value::Array(values) => values.iter_mut().for_each(materialize),
      Value::Object(fields) => {
        for (key, value) in fields.iter_mut() {
          if key == "pid" {
            if let Some(number) = value.as_str().and_then(|text| text.strip_prefix("$PID_")) {
              *value = Value::from(number.parse::<u64>().unwrap());
              continue;
            }
          }
          materialize(value);
        }
      }
      _ => {}
    }
  }
  records.iter_mut().for_each(materialize);
}
