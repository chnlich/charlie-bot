use std::fs;
use std::path::Path;
use std::process::Command;

use charliebot_usage_page::UsageLedger;
use flate2::read::GzDecoder;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use tempfile::tempdir;

const TRIGGER_NAMES: [&str; 5] = [
  "usage_agg_after_insert",
  "usage_agg_after_update",
  "usage_agg_after_delete",
  "usage_agg_after_native_session",
  "usage_agg_after_fallback_session",
];

#[test]
fn recorded_python_usage_ledger_schema_and_repair_results_match_rust() {
  let fixture = fixture();
  assert_eq!(fixture["source"]["helper"], "rust/crates/features/usage/tests/shared_file.py");
  assert_eq!(fixture["source"]["base_commit"], "a572e7ca71858d9ba52f0abc2726fa44213210ac");
  assert_eq!(fixture["python_helper_error"], "AttributeError: 'UsageLedger' object has no attribute 'model_rows'");

  let temp = tempdir().unwrap();
  let python_path = temp.path().join("python.sqlite3");
  write_python_database(&fixture, &python_path, temp.path());
  let before = trigger_and_aggregate(&python_path);
  UsageLedger::open(&python_path).unwrap().close().unwrap();
  assert_eq!(before, trigger_and_aggregate(&python_path));
  assert_eq!(before, fixture["python_created_snapshot"]);

  let rust_path = temp.path().join("rust.sqlite3");
  let output =
    Command::new(env!("CARGO_BIN_EXE_usage-fixture")).args(["fill", rust_path.to_str().unwrap()]).output().unwrap();
  assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
  let rust_before = trigger_and_aggregate(&rust_path);
  UsageLedger::open(&rust_path).unwrap().close().unwrap();
  let rust_after = trigger_and_aggregate(&rust_path);
  assert_eq!(rust_before, rust_after);
  assert_eq!(rust_before, fixture["rust_created_snapshot"]);
  assert_eq!(rust_after, fixture["rust_opened_rust_snapshot"]);

  let damaged_path = temp.path().join("damaged.sqlite3");
  write_python_database(&fixture, &damaged_path, temp.path());
  {
    let connection = Connection::open(&damaged_path).unwrap();
    for name in TRIGGER_NAMES {
      connection
        .execute_batch(&format!(
          "DROP TRIGGER {name}; CREATE TRIGGER {name} AFTER INSERT ON usage BEGIN SELECT 1; END;"
        ))
        .unwrap();
    }
    connection.execute("DELETE FROM usage_agg", []).unwrap();
    connection
      .execute("INSERT INTO usage_agg VALUES ('wrong', 'wrong', 'wrong', 'native', '2020-01-01', 9, 9, 9, 9, 9, 9)", [])
      .unwrap();
  }
  UsageLedger::open(&damaged_path).unwrap().close().unwrap();
  assert_eq!(trigger_and_aggregate(&damaged_path), fixture["python_repaired_snapshot"]);
}

fn fixture() -> Value {
  let compressed = include_bytes!("fixtures/python_usage_shared_file.json.gz");
  let mut decoder = GzDecoder::new(compressed.as_slice());
  let mut bytes = Vec::new();
  std::io::Read::read_to_end(&mut decoder, &mut bytes).unwrap();
  serde_json::from_slice(&bytes).unwrap()
}

fn write_python_database(fixture: &Value, path: &Path, temp_root: &Path) {
  fs::write(path, decode_hex(fixture["python_created_ledger_hex"].as_str().unwrap())).unwrap();
  let connection = Connection::open(path).unwrap();
  let rooted = temp_root.join("sessions/log.jsonl").to_string_lossy().into_owned();
  connection
    .execute("UPDATE usage SET origin = ?1 WHERE origin = '$TMPROOT/sessions/log.jsonl'", params![rooted])
    .unwrap();
  connection
    .execute("UPDATE captured_files SET path = ?1 WHERE path = '$TMPROOT/sessions/log.jsonl'", params![rooted])
    .unwrap();
}

fn trigger_and_aggregate(path: &Path) -> Value {
  let connection = Connection::open(path).unwrap();
  let triggers = connection
    .prepare("SELECT name, sql FROM sqlite_master WHERE type = 'trigger' ORDER BY name")
    .unwrap()
    .query_map([], |row| Ok(json!([row.get::<_, String>(0)?, row.get::<_, String>(1)?])))
    .unwrap()
    .map(Result::unwrap)
    .collect::<Vec<_>>();
  let aggregate = connection
    .prepare("SELECT * FROM usage_agg ORDER BY source, model, account, kind, day")
    .unwrap()
    .query_map([], |row| {
      let values = (0..row.as_ref().column_count())
        .map(|index| row.get::<_, rusqlite::types::Value>(index).map(sql_value))
        .collect::<rusqlite::Result<Vec<_>>>()?;
      Ok(Value::Array(values))
    })
    .unwrap()
    .map(Result::unwrap)
    .collect::<Vec<_>>();
  json!({"triggers": triggers, "aggregate": aggregate})
}

fn sql_value(value: rusqlite::types::Value) -> Value {
  match value {
    rusqlite::types::Value::Null => Value::Null,
    rusqlite::types::Value::Integer(value) => json!(value),
    rusqlite::types::Value::Real(value) => json!(value),
    rusqlite::types::Value::Text(value) => Value::String(value),
    rusqlite::types::Value::Blob(value) => Value::Array(value.into_iter().map(|byte| json!(byte)).collect()),
  }
}

fn decode_hex(value: &str) -> Vec<u8> {
  value
    .as_bytes()
    .as_chunks::<2>()
    .0
    .iter()
    .map(|pair| {
      let high = (pair[0] as char).to_digit(16).unwrap();
      let low = (pair[1] as char).to_digit(16).unwrap();
      ((high << 4) | low) as u8
    })
    .collect()
}
