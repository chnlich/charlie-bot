use std::collections::BTreeSet;
use std::ffi::CStr;
use std::fs::{self, File};
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime};

use flate2::read::GzDecoder;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, Row};
use serde_json::{Map, Value, json};
use tempfile::tempdir;

const NOW: &str = "2026-10-19T12:00:00+00:00";
const NOW_UNIX: u64 = 1_792_411_200;
const LEDGER_DIR: &str = "charlie/usage";

#[test]
fn recorded_python_cold_storage_outputs_match_all_tree_and_ledger_fields() {
  let fixture = fixture();
  assert_eq!(fixture["source"]["helper"], "rust/crates/features/usage/tests/cool_storage_differential.py");
  assert_eq!(fixture["source"]["base_commit"], "a572e7ca71858d9ba52f0abc2726fa44213210ac");
  assert_eq!(fixture["source"]["now"], "$NOW");
  assert_eq!(fixture["deleted_count"], 39);
  assert_eq!(fixture["python_rust_differences"].as_array().unwrap().len(), 1);
  assert!(fixture["python_rust_differences"][0].as_str().unwrap().contains("ledger table usage differs at row 3"));
  assert_eq!(fixture["control_differences"].as_array().unwrap().len(), 2);
  assert_eq!(fixture["refusal_python_error"], "AttributeError: 'list' object has no attribute 'get'");

  let hostname = hostname();
  let temp = tempdir().unwrap();
  let run = temp.path().join("run");
  materialize(&fixture, &run);
  let summary = sweep(&run);
  let actual = state(&run, &summary, &hostname);
  assert_state_matches_recorded_origin(
    &fixture["rust_state"],
    &fixture["python_state"],
    &actual,
    "recorded Rust state",
  );
  let baseline_differences = state_difference_count(&fixture["python_state"], &actual);
  assert!(baseline_differences <= 1, "only the recorded duplicate transcript path may differ");

  let control = temp.path().join("control");
  materialize(&fixture, &control);
  let control_summary = sweep(&control);
  let control_baseline = state(&control, &control_summary, &hostname);
  let control_baseline_differences = state_difference_count(&fixture["python_state"], &control_baseline);
  assert!(control_baseline_differences <= 1, "only the recorded duplicate transcript path may differ");
  let planted = control.join(fixture["control_path"].as_str().unwrap());
  let mut file = fs::OpenOptions::new().append(true).open(planted).unwrap();
  use std::io::Write;
  file.write_all(b"planted difference\n").unwrap();
  let control_state = state(&control, &control_summary, &hostname);
  assert_state_matches_recorded_origin(
    &fixture["control_state"],
    &fixture["python_state"],
    &control_state,
    "recorded control state",
  );
  assert_eq!(state_difference_count(&control_baseline, &control_state), 1);
  assert_eq!(fixture["control_differences"][0], format!("bytes differ: {}", fixture["control_path"].as_str().unwrap()));

  let refusal = temp.path().join("refusal");
  materialize_refusal(&fixture, &refusal);
  let before = file_tree(&refusal, &hostname);
  let output = sweep_output(&refusal);
  assert!(!output.status.success());
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert_eq!(stderr.lines().last().unwrap(), fixture["refusal_rust_error"].as_str().unwrap());
  assert_eq!(file_tree(&refusal, &hostname), before, "refusal leaves all files untouched");
}

fn fixture() -> Value {
  let compressed = include_bytes!("fixtures/python_cool_storage.json.gz");
  let mut decoder = GzDecoder::new(compressed.as_slice());
  let mut bytes = Vec::new();
  std::io::Read::read_to_end(&mut decoder, &mut bytes).unwrap();
  serde_json::from_slice(&bytes).unwrap()
}

fn materialize(fixture: &Value, root: &Path) {
  for relative in fixture["input_dirs"].as_array().unwrap() {
    fs::create_dir_all(root.join(fill_path(relative.as_str().unwrap(), root))).unwrap();
  }
  for (relative, encoded) in fixture["input_files"].as_object().unwrap() {
    let path = root.join(fill_path(relative, root));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let bytes = replace_bytes(&decode_hex(encoded.as_str().unwrap()), b"$TMPROOT", root.to_string_lossy().as_bytes());
    fs::write(&path, bytes).unwrap();
  }
  for (relative, target) in fixture["symlinks"].as_object().unwrap() {
    let link = root.join(fill_path(relative, root));
    fs::create_dir_all(link.parent().unwrap()).unwrap();
    symlink(target.as_str().unwrap(), link).unwrap();
  }
  for (relative, seconds_before) in fixture["mtime_seconds_before_now"].as_object().unwrap() {
    let path = root.join(fill_path(relative, root));
    let seconds_before = seconds_before.as_u64().unwrap();
    let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(NOW_UNIX - seconds_before);
    File::options()
      .write(true)
      .open(path)
      .unwrap()
      .set_times(std::fs::FileTimes::new().set_modified(modified))
      .unwrap();
  }
}

fn materialize_refusal(fixture: &Value, root: &Path) {
  for (relative, encoded) in fixture["refusal_files"].as_object().unwrap() {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let bytes = replace_bytes(&decode_hex(encoded.as_str().unwrap()), b"$TMPROOT", root.to_string_lossy().as_bytes());
    fs::write(path, bytes).unwrap();
  }
}

fn fill_path(path: &str, root: &Path) -> String {
  path.replace("$ROOT_ENCODED", &encoded_path(root))
}

fn encoded_path(path: &Path) -> String {
  path.to_string_lossy().replace(['/', '.', '_'], "-")
}

fn sweep(root: &Path) -> String {
  let output = sweep_output(root);
  assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
  let stdout = String::from_utf8(output.stdout).unwrap();
  let summary = stdout.lines().last().unwrap().to_string();
  assert!(summary.starts_with("total "), "{stdout}");
  summary
}

fn sweep_output(root: &Path) -> std::process::Output {
  Command::new(env!("CARGO_BIN_EXE_cool-storage-sweep"))
    .arg(root)
    .arg(NOW)
    .env("HOME", root.join("home"))
    .env("CHARLIEBOT_HOME", root.join("charlie"))
    .env("CLAUDE_CONFIG_DIR", root.join("claude-config"))
    .output()
    .unwrap()
}

fn state(root: &Path, summary: &str, hostname: &str) -> Value {
  json!({
    "files": file_tree(root, hostname),
    "dirs": directories(root),
    "ledger": ledger_tables(root, hostname),
    "summary": summary,
  })
}

fn file_tree(root: &Path, hostname: &str) -> Value {
  let mut files = Map::new();
  collect_files(root, root, hostname, &mut files);
  Value::Object(files)
}

fn collect_files(root: &Path, directory: &Path, hostname: &str, files: &mut Map<String, Value>) {
  for entry in fs::read_dir(directory).unwrap() {
    let path = entry.unwrap().path();
    let relative = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
    if relative.starts_with(&format!("{LEDGER_DIR}/")) {
      continue;
    }
    let file_type = fs::symlink_metadata(&path).unwrap().file_type();
    if file_type.is_symlink() {
      continue;
    }
    if path.is_dir() {
      collect_files(root, &path, hostname, files);
    } else if path.is_file() {
      let bytes = fs::read(&path).unwrap();
      let bytes = replace_bytes(&bytes, root.to_string_lossy().as_bytes(), b"$TMPROOT");
      let bytes = replace_bytes(&bytes, hostname.as_bytes(), b"$HOSTNAME");
      files.insert(normalize_text(&relative, root, hostname), Value::String(hex(&bytes)));
    }
  }
}

fn directories(root: &Path) -> Value {
  let mut values = BTreeSet::new();
  collect_directories(root, root, &mut values);
  Value::Array(values.into_iter().map(Value::String).collect())
}

fn collect_directories(root: &Path, directory: &Path, values: &mut BTreeSet<String>) {
  for entry in fs::read_dir(directory).unwrap() {
    let path = entry.unwrap().path();
    let relative = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
    if relative.starts_with(LEDGER_DIR) {
      continue;
    }
    let file_type = fs::symlink_metadata(&path).unwrap().file_type();
    if file_type.is_symlink() {
      if fs::metadata(&path).unwrap().is_dir() {
        values.insert(normalize_text(&relative, root, ""));
      }
    } else if path.is_dir() {
      values.insert(normalize_text(&relative, root, ""));
      collect_directories(root, &path, values);
    }
  }
}

fn ledger_tables(root: &Path, hostname: &str) -> Value {
  let connection = Connection::open(root.join("charlie/usage/ledger.sqlite3")).unwrap();
  let mut tables = Map::new();
  for (table, columns, order) in [
    (
      "usage",
      "record_id, kind, source, model, account, host, ts, in_fresh, cache_write, cache_read, in_unsplit, output, origin",
      "record_id",
    ),
    ("native_sessions", "*", "session, source"),
    ("fallback_sessions", "*", "record_id, session"),
    ("captured_files", "*", "host, path"),
    ("usage_agg", "*", "source, model, account, kind, day"),
    ("ledger_meta", "*", "key"),
  ] {
    let mut statement = connection.prepare(&format!("SELECT {columns} FROM {table} ORDER BY {order}")).unwrap();
    let count = statement.column_count();
    let mut rows = statement.query_map([], |row| row_json(row, count)).unwrap().map(Result::unwrap).collect::<Vec<_>>();
    for row in &mut rows {
      normalize_table_row(table, row, root, hostname);
    }
    tables.insert(table.to_string(), Value::Array(rows));
  }
  Value::Object(tables)
}

fn normalize_table_row(table: &str, row: &mut Value, root: &Path, hostname: &str) {
  match table {
    "usage" => {
      let path = row[12].as_str().unwrap();
      row[12] = Value::String(normalize_text(path, root, hostname));
      let host = row[5].as_str().unwrap();
      row[5] = Value::String(normalize_text(host, root, hostname));
    }
    "captured_files" => {
      let path = row[1].as_str().unwrap();
      row[1] = Value::String(normalize_text(path, root, hostname));
      let host = row[0].as_str().unwrap();
      row[0] = Value::String(normalize_text(host, root, hostname));
    }
    "ledger_meta" => {
      let key = normalize_text(row[0].as_str().unwrap(), root, hostname);
      row[0] = Value::String(key.clone());
      if key.starts_with("last_capture_at:") {
        row[1] = json!("<timestamp omitted>");
      }
    }
    _ => {
      for value in row.as_array_mut().unwrap() {
        if let Some(text) = value.as_str() {
          *value = Value::String(normalize_text(text, root, hostname));
        }
      }
    }
  }
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

fn normalize_text(value: &str, root: &Path, hostname: &str) -> String {
  value
    .replace(root.to_str().unwrap(), "$TMPROOT")
    .replace(&encoded_path(root), "$ROOT_ENCODED")
    .replace(hostname, if hostname.is_empty() { "" } else { "$HOSTNAME" })
}

fn state_difference_count(left: &Value, right: &Value) -> usize {
  let mut count = usize::from(left["summary"] != right["summary"]);
  let left_files = left["files"].as_object().unwrap();
  let right_files = right["files"].as_object().unwrap();
  let file_keys = left_files.keys().chain(right_files.keys()).cloned().collect::<BTreeSet<_>>();
  count += file_keys.iter().filter(|key| left_files.get(key.as_str()) != right_files.get(key.as_str())).count();
  let left_dirs =
    left["dirs"].as_array().unwrap().iter().map(|value| value.as_str().unwrap().to_string()).collect::<BTreeSet<_>>();
  let right_dirs =
    right["dirs"].as_array().unwrap().iter().map(|value| value.as_str().unwrap().to_string()).collect::<BTreeSet<_>>();
  count += left_dirs.symmetric_difference(&right_dirs).count();
  for table in ["usage", "native_sessions", "fallback_sessions", "captured_files", "usage_agg", "ledger_meta"] {
    count += usize::from(left["ledger"][table] != right["ledger"][table]);
  }
  count
}

fn assert_state_eq(expected: &Value, actual: &Value, label: &str) {
  assert_eq!(first_difference(expected, actual, "state"), None, "{label}");
}

fn assert_state_matches_recorded_origin(expected_rust: &Value, expected_python: &Value, actual: &Value, label: &str) {
  let python_origin = expected_python["ledger"]["usage"][3][12].as_str().unwrap();
  let rust_origin = expected_rust["ledger"]["usage"][3][12].as_str().unwrap();
  let actual_origin = actual["ledger"]["usage"][3][12].as_str().unwrap();
  assert!(
    actual_origin == python_origin || actual_origin == rust_origin,
    "unexpected duplicate transcript path {actual_origin}"
  );
  let mut expected = expected_rust.clone();
  expected["ledger"]["usage"][3][12] = Value::String(actual_origin.to_string());
  assert_state_eq(&expected, actual, label);
}

fn first_difference(expected: &Value, actual: &Value, path: &str) -> Option<String> {
  match (expected, actual) {
    (Value::Object(expected), Value::Object(actual)) => {
      for key in expected.keys().chain(actual.keys()).collect::<BTreeSet<_>>() {
        match (expected.get(key), actual.get(key)) {
          (Some(expected), Some(actual)) => {
            if let Some(difference) = first_difference(expected, actual, &format!("{path}.{key}")) {
              return Some(difference);
            }
          }
          (Some(_), None) => return Some(format!("{path}.{key} is missing")),
          (None, Some(_)) => return Some(format!("{path}.{key} is unexpected")),
          (None, None) => unreachable!(),
        }
      }
      None
    }
    (Value::Array(expected), Value::Array(actual)) => {
      if expected.len() != actual.len() {
        return Some(format!("{path} length {} != {}", expected.len(), actual.len()));
      }
      expected
        .iter()
        .zip(actual)
        .enumerate()
        .find_map(|(index, (expected, actual))| first_difference(expected, actual, &format!("{path}[{index}]")))
    }
    _ if expected == actual => None,
    _ => Some(format!("{path}: expected {expected}, actual {actual}")),
  }
}

fn replace_bytes(value: &[u8], old: &[u8], new: &[u8]) -> Vec<u8> {
  if old.is_empty() {
    return value.to_vec();
  }
  let mut output = Vec::new();
  let mut start = 0;
  while let Some(offset) = value[start..].windows(old.len()).position(|window| window == old) {
    let index = start + offset;
    output.extend_from_slice(&value[start..index]);
    output.extend_from_slice(new);
    start = index + old.len();
  }
  output.extend_from_slice(&value[start..]);
  output
}

fn decode_hex(value: &str) -> Vec<u8> {
  value
    .as_bytes()
    .chunks_exact(2)
    .map(|pair| {
      let high = (pair[0] as char).to_digit(16).unwrap();
      let low = (pair[1] as char).to_digit(16).unwrap();
      ((high << 4) | low) as u8
    })
    .collect()
}

fn hex(value: &[u8]) -> String {
  value.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hostname() -> String {
  let mut name = [0_i8; 256];
  let result = unsafe { libc::gethostname(name.as_mut_ptr(), name.len()) };
  assert_eq!(result, 0, "gethostname");
  unsafe { CStr::from_ptr(name.as_ptr()) }.to_string_lossy().into_owned()
}
