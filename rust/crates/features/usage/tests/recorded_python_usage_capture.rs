use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use flate2::read::GzDecoder;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, Row};
use serde_json::{Map, Value, json};
use tempfile::tempdir;

const FIXED_MTIME_NS: u64 = 1_700_000_000_123_456_789;

#[test]
fn recorded_python_capture_matches_rows_and_all_ledger_tables() {
  let fixture = fixture();
  assert_eq!(fixture["source"]["helper"], "rust/crates/features/usage/tests/differential.py");
  assert_eq!(fixture["source"]["base_commit"], "a572e7ca71858d9ba52f0abc2726fa44213210ac");
  assert_eq!(fixture["source"]["mtime_ns"], "$FIXED_MTIME_NS");

  let root = tempdir().unwrap();
  let home = root.path().join("synthetic-home");
  for (relative, encoded) in fixture["input_files"].as_object().unwrap() {
    let path = home.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, decode_hex(encoded.as_str().unwrap())).unwrap();
  }
  let modified =
    SystemTime::UNIX_EPOCH + Duration::new(FIXED_MTIME_NS / 1_000_000_000, (FIXED_MTIME_NS % 1_000_000_000) as u32);
  for entry in fs::read_dir(&home).unwrap() {
    set_mtime_recursive(&entry.unwrap().path(), modified);
  }
  let live_ids = home.join("codex/live_cli_sessions.txt");
  let ledger = root.path().join("rust-ledger/ledger.sqlite3");
  let output = Command::new(env!("CARGO_BIN_EXE_usage-fixture"))
    .args(["capture", home.to_str().unwrap(), ledger.to_str().unwrap()])
    .env("HOME", &home)
    .env("CHARLIEBOT_HOME", &home)
    .env("FIXTURE_LIVE_SESSIONS", &live_ids)
    .output()
    .unwrap();
  assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
  let rust_capture: Value = serde_json::from_slice(&output.stdout).unwrap();
  assert_eq!(rust_capture["rows"], fixture["python_capture"]["rows"]);

  let rust_snapshot = snapshot(&home, &ledger);
  assert_eq!(rust_snapshot, fixture["python_snapshot"]);

  let mut control = rust_snapshot.clone();
  control["usage_agg"][0][10] = json!(control["usage_agg"][0][10].as_f64().unwrap() + 1.0);
  let changed_tables = fixture["python_snapshot"]
    .as_object()
    .unwrap()
    .keys()
    .filter(|table| control[*table] != fixture["python_snapshot"][*table])
    .count();
  assert_eq!(changed_tables, 1);
  assert!(fixture["control_difference"].as_str().unwrap().starts_with("usage_agg:"));
}

fn fixture() -> Value {
  let compressed = include_bytes!("fixtures/python_usage_capture.json.gz");
  let mut decoder = GzDecoder::new(compressed.as_slice());
  let mut bytes = Vec::new();
  std::io::Read::read_to_end(&mut decoder, &mut bytes).unwrap();
  serde_json::from_slice(&bytes).unwrap()
}

fn snapshot(home: &Path, ledger: &Path) -> Value {
  let connection = Connection::open(ledger).unwrap();
  let mut tables = Map::new();
  for (table, order) in [
    ("usage", "record_id"),
    ("native_sessions", "session, source"),
    ("fallback_sessions", "record_id, session"),
    ("captured_files", "host, path"),
    ("usage_agg", "source, model, account, kind, day"),
    ("ledger_meta", "key"),
  ] {
    let mut statement = connection.prepare(&format!("SELECT * FROM {table} ORDER BY {order}")).unwrap();
    let columns = statement.column_count();
    let rows = statement.query_map([], |row| row_json(row, columns)).unwrap().map(Result::unwrap).collect::<Vec<_>>();
    tables.insert(table.to_string(), Value::Array(rows));
  }
  normalize_snapshot_paths(home, &mut tables);
  Value::Object(tables)
}

fn row_json(row: &Row<'_>, columns: usize) -> rusqlite::Result<Value> {
  (0..columns).map(|index| row.get_ref(index).map(sql_value)).collect::<rusqlite::Result<Vec<_>>>().map(Value::Array)
}

fn sql_value(value: ValueRef<'_>) -> Value {
  match value {
    ValueRef::Null => Value::Null,
    ValueRef::Integer(value) => json!(value),
    ValueRef::Real(value) => json!(value),
    ValueRef::Text(value) => Value::String(String::from_utf8_lossy(value).into_owned()),
    ValueRef::Blob(value) => Value::Array(value.iter().map(|byte| json!(byte)).collect()),
  }
}

fn normalize_snapshot_paths(home: &Path, tables: &mut Map<String, Value>) {
  for row in tables["usage"].as_array_mut().unwrap() {
    let path = PathBuf::from(row[12].as_str().unwrap());
    row[12] = Value::String(path.strip_prefix(home).unwrap().to_string_lossy().replace('\\', "/"));
    row.as_array_mut().unwrap().truncate(13);
  }
  for row in tables["captured_files"].as_array_mut().unwrap() {
    let path = PathBuf::from(row[1].as_str().unwrap());
    row[1] = Value::String(path.strip_prefix(home).unwrap().to_string_lossy().replace('\\', "/"));
  }
  for row in tables["ledger_meta"].as_array_mut().unwrap() {
    let key = row[0].as_str().unwrap();
    if key.starts_with("last_capture_at:") {
      row[1] = json!("<timestamp omitted>");
    }
  }
}

fn set_mtime_recursive(path: &Path, modified: SystemTime) {
  if path.is_dir() {
    for entry in fs::read_dir(path).unwrap() {
      set_mtime_recursive(&entry.unwrap().path(), modified);
    }
  } else {
    File::options()
      .write(true)
      .open(path)
      .unwrap()
      .set_times(std::fs::FileTimes::new().set_modified(modified))
      .unwrap();
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
