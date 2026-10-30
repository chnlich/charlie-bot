//! The opencode usage source checked against its Python reference (`opencode_usage_reference.py`). The synthetic stores
//! are written once to disk. Each step reads one store with the signature the step names: first Rust reads every step,
//! then Python reads the same files, and each step's records and signature must be equal. No store's main file changes
//! under either reader.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use charliebot_opencode::usage_logs::read;
use charliebot_registry::usage::{RecordKind, UsageRecord};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use tempfile::TempDir;

const PYTHON_FIXTURE: &str = include_str!("fixtures/opencode_usage_golden.json");

const ACCOUNT: &str = "default";

/// 2026-10-14T17:46:40 UTC in milliseconds.
const CREATED_MS: i64 = 1_792_000_000_000;

const SCHEMA: &str = "create table session_message(id TEXT PRIMARY KEY, session_id TEXT, type TEXT, seq INTEGER, \
                      time_created INTEGER, time_updated INTEGER, data TEXT)";

/// One `session_message` row: id, session id, role, time_updated and the data text.
type Row = (&'static str, &'static str, &'static str, i64, String);

/// The previous signature of a step: none, a literal in some other format, or the signature a earlier step returned.
enum Previous {
  Absent,
  Literal(&'static str),
  Step(usize),
}

fn model_a() -> Value {
  json!({"id": "model-a", "providerID": "provider-a"})
}

fn model_b() -> Value {
  json!({"id": "model-b", "providerID": "provider-b"})
}

fn tokens(input: Value, output: Value) -> Value {
  json!({"input": input, "output": output})
}

/// An assistant blob at the shared created time, with the given model and tokens.
fn blob(model: Value, tokens: Value) -> String {
  json!({"time": {"created": CREATED_MS}, "model": model, "tokens": tokens}).to_string()
}

/// The matrix rows in insertion order, which is the table's rowid order. Each row's time_updated is its index plus 100,
/// so the last row holds the store's max.
fn matrix_rows() -> Vec<Row> {
  let entries: Vec<(&str, &str, &str, String)> = vec![
    ("user-row", "sess-a", "user", blob(model_a(), tokens(json!(70), json!(7)))),
    (
      "basic",
      "sess-a",
      "assistant",
      json!({
        "time": {"created": CREATED_MS + 123},
        "model": model_a(),
        "tokens": {"input": 100, "output": 30, "reasoning": 0, "cache": {"read": 5, "write": 7}},
      })
      .to_string(),
    ),
    (
      "whole",
      "sess-b",
      "assistant",
      json!({"time": {"created": CREATED_MS}, "model": model_b(), "tokens": {"input": 50, "output": 5}}).to_string(),
    ),
    (
      "float-ms",
      "sess-a",
      "assistant",
      json!({"time": {"created": 1792000000123.5}, "model": model_a(), "tokens": {"input": 1, "output": 1}})
        .to_string(),
    ),
    (
      "bool-created",
      "sess-a",
      "assistant",
      json!({"time": {"created": true}, "model": {"id": "model-d", "providerID": "provider-d"}, "tokens": {"input": 2, "output": 2}})
        .to_string(),
    ),
    (
      "negative-created",
      "sess-a",
      "assistant",
      json!({"time": {"created": -1}, "model": model_a(), "tokens": {"input": 2, "output": 2}}).to_string(),
    ),
    (
      "string-created",
      "sess-a",
      "assistant",
      json!({"time": {"created": "1792000000000"}, "model": model_a(), "tokens": {"input": 2, "output": 2}})
        .to_string(),
    ),
    (
      "null-created",
      "sess-a",
      "assistant",
      json!({"time": {"created": null}, "model": model_a(), "tokens": {"input": 2, "output": 2}}).to_string(),
    ),
    ("no-time", "sess-a", "assistant", json!({"model": model_a(), "tokens": {"input": 3, "output": 3}}).to_string()),
    (
      "time-not-object",
      "sess-a",
      "assistant",
      json!({"time": 5, "model": model_a(), "tokens": {"input": 3, "output": 3}}).to_string(),
    ),
    (
      "zero-tokens",
      "sess-a",
      "assistant",
      blob(model_a(), json!({"input": 0, "output": 0, "cache": {"read": 9, "write": 9}})),
    ),
    ("error-row", "sess-a", "assistant", json!({"error": "boom", "model": model_a()}).to_string()),
    ("escaped-key", "sess-a", "assistant", r#"{"tokens":{"input":5,"output":1}}"#.to_string()),
    ("uppercase-key", "sess-a", "assistant", r#"{"TOKENS":{"input":4,"output":2}}"#.to_string()),
    ("truncated", "sess-a", "assistant", r#"{"tokens":{"input":5,"output":1}"#.to_string()),
    ("nan-count", "sess-a", "assistant", r#"{"tokens":{"input":NaN,"output":1}}"#.to_string()),
    ("infinity-count", "sess-a", "assistant", r#"{"tokens":{"input":1,"output":-Infinity}}"#.to_string()),
    ("array-blob", "sess-a", "assistant", r#"["tokens",{"input":1,"output":1}]"#.to_string()),
    ("tokens-string", "sess-a", "assistant", r#"{"tokens":"tokens"}"#.to_string()),
    ("cache-string", "sess-a", "assistant", blob(model_a(), json!({"input": 8, "output": 2, "cache": "x"}))),
    ("zero-input", "sess-a", "assistant", blob(model_a(), tokens(json!(0), json!(4)))),
    ("null-counts", "sess-a", "assistant", blob(model_a(), tokens(Value::Null, Value::Null))),
    (
      "empty-model",
      "sess-a",
      "assistant",
      blob(json!({"id": "", "providerID": ""}), tokens(json!(3), json!(0))),
    ),
    (
      "absent-model",
      "sess-a",
      "assistant",
      json!({"time": {"created": CREATED_MS}, "tokens": {"input": 3, "output": 0}}).to_string(),
    ),
    (
      "slash-model",
      "sess-a",
      "assistant",
      blob(json!({"id": "/models/gpt-5/", "providerID": "openai"}), tokens(json!(6), json!(6))),
    ),
    ("slash-only", "sess-a", "assistant", blob(json!({"id": "/", "providerID": "p"}), tokens(json!(7), json!(7)))),
    (
      "slash-dots",
      "sess-a",
      "assistant",
      blob(json!({"id": "/a/./b/..", "providerID": ""}), tokens(json!(8), json!(8))),
    ),
    (
      "unicode-model",
      "sess-a",
      "assistant",
      blob(json!({"id": "/x/模型", "providerID": "代理"}), tokens(json!(9), json!(9))),
    ),
    (
      "cache-falsy",
      "sess-a",
      "assistant",
      blob(model_a(), json!({"input": 10, "output": 10, "cache": {"read": null, "write": false}})),
    ),
    ("duplicate-key", "sess-a", "assistant", r#"{"tokens":{"input":1,"input":9,"output":2}}"#.to_string()),
    (
      "raw-newline",
      "sess-a",
      "assistant",
      "{\"tokens\":{\"input\":3,\"output\":3},\"model\":{\"id\":\"a\nb\"}}".to_string(),
    ),
  ];
  entries
    .into_iter()
    .enumerate()
    .map(|(index, (id, session, role, data))| (id, session, role, 100 + index as i64, data))
    .collect()
}

fn create_store(db: &Path, rows: &[Row]) {
  let mut connection = Connection::open(db).unwrap();
  connection.execute(SCHEMA, []).unwrap();
  insert_rows(&mut connection, rows);
}

/// A WAL-mode store whose WAL file stays beside it while the returned connection is open.
fn create_wal_store(db: &Path, rows: &[Row]) -> Connection {
  let mut connection = Connection::open(db).unwrap();
  connection.pragma_update(None, "journal_mode", "WAL").unwrap();
  connection.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
  connection.execute(SCHEMA, []).unwrap();
  insert_rows(&mut connection, rows);
  connection
}

fn insert_rows(connection: &mut Connection, rows: &[Row]) {
  let transaction = connection.transaction().unwrap();
  for (id, session, role, updated, data) in rows {
    transaction
      .execute(
        "insert into session_message (id, session_id, type, seq, time_created, time_updated, data) \
         values (?, ?, ?, 0, 1700000000000, ?, ?)",
        params![id, session, role, updated, data],
      )
      .unwrap();
  }
  transaction.commit().unwrap();
}

fn python_reference(request: &Value) -> Value {
  let fixture: Value = serde_json::from_str(PYTHON_FIXTURE).unwrap();
  assert_eq!(fixture["source"], "rust/crates/runtime/agents/tests/opencode_usage_reference.py");
  assert_eq!(fixture["base_commit"], "a572e7ca71858d9ba52f0abc2726fa44213210ac");
  let mut expected = fixture["expected"].clone();
  for (index, step) in request["steps"].as_array().unwrap().iter().enumerate() {
    let database = Path::new(step["path"].as_str().unwrap());
    let mut fields =
      expected["steps"][index]["signature"].as_str().unwrap().split(':').map(str::to_string).collect::<Vec<_>>();
    let db = fs::metadata(database).unwrap();
    let db_mtime_ns = i128::from(db.mtime()) * 1_000_000_000 + i128::from(db.mtime_nsec());
    fields[1] = db_mtime_ns.to_string();
    if fields[3] != "0" {
      let wal_path = PathBuf::from(format!("{}-wal", database.display()));
      let wal = fs::metadata(wal_path).unwrap();
      fields[3] = (i128::from(wal.mtime()) * 1_000_000_000 + i128::from(wal.mtime_nsec())).to_string();
    }
    expected["steps"][index]["signature"] = json!(fields.join(":"));
  }
  expected
}

fn record_json(record: &UsageRecord) -> Value {
  json!({
    "record_id": record.record_id,
    "kind": match record.kind {
      RecordKind::Native => "native",
      RecordKind::Fallback => "fallback",
    },
    "source": record.source,
    "model": record.model,
    "account": record.account,
    "ts": record.ts,
    "in_fresh": record.in_fresh,
    "cache_write": record.cache_write,
    "cache_read": record.cache_read,
    "output": record.output,
    "in_unsplit": record.in_unsplit,
    "sessions": record.sessions,
  })
}

/// Each store's main file: its bytes and mtime, which no read may change.
fn main_files(stores: &BTreeMap<&str, PathBuf>) -> BTreeMap<PathBuf, (Vec<u8>, SystemTime)> {
  stores
    .values()
    .map(|path| (path.clone(), (fs::read(path).unwrap(), fs::metadata(path).unwrap().modified().unwrap())))
    .collect()
}

#[test]
fn read_matches_the_python_usage_log_over_synthetic_stores() {
  let root = TempDir::new().unwrap();
  let rows = matrix_rows();
  let floor = rows.iter().map(|row| row.3).max().unwrap();
  let updated_of = |id: &str| rows.iter().find(|row| row.0 == id).unwrap().3;
  let moved: Vec<Row> = vec![
    // Below the stored max, so the read skips it although its content changed.
    ("basic", "sess-a", "assistant", updated_of("basic"), blob(model_a(), tokens(json!(999), json!(999)))),
    ("boundary", "sess-a", "assistant", floor, blob(model_a(), tokens(json!(13), json!(13)))),
    ("float-ms", "sess-a", "assistant", floor + 20, blob(model_a(), tokens(json!(14), json!(14)))),
    ("new-row", "sess-b", "assistant", floor + 1, blob(model_b(), tokens(json!(15), json!(15)))),
    ("user-new", "sess-b", "user", floor + 2, blob(model_a(), tokens(json!(16), json!(16)))),
  ];
  let wal_rows: Vec<Row> = vec![("wal-row", "sess-a", "assistant", 100, blob(model_a(), tokens(json!(11), json!(12))))];

  let stores = BTreeMap::from([
    ("rows", root.path().join("rows.db")),
    ("moved", root.path().join("moved.db")),
    ("empty", root.path().join("empty.db")),
    ("wal", root.path().join("wal.db")),
  ]);
  create_store(&stores["rows"], &rows);
  create_store(&stores["moved"], &moved);
  create_store(&stores["empty"], &[]);
  // The writer stays open until the last read, so the WAL file exists for both readers.
  let _wal_writer = create_wal_store(&stores["wal"], &wal_rows);

  let steps: Vec<(&str, Previous)> = vec![
    ("rows", Previous::Absent),
    ("rows", Previous::Step(0)),
    ("moved", Previous::Step(0)),
    ("moved", Previous::Step(2)),
    ("rows", Previous::Literal("legacy-gate-signature")),
    ("rows", Previous::Literal("1:2:3:4")),
    ("empty", Previous::Absent),
    ("wal", Previous::Absent),
    ("wal", Previous::Step(7)),
  ];
  let before = main_files(&stores);

  let mut outcomes: Vec<(String, Vec<UsageRecord>)> = Vec::new();
  let mut previous_signatures: Vec<Option<String>> = Vec::new();
  for (store, previous) in &steps {
    let previous = match previous {
      Previous::Absent => None,
      Previous::Literal(signature) => Some((*signature).to_string()),
      Previous::Step(index) => Some(outcomes[*index].0.clone()),
    };
    let (signature, records) = read(&stores[store], ACCOUNT, previous.as_deref()).unwrap();
    previous_signatures.push(previous);
    outcomes.push((signature, records));
  }

  let request = json!({
    "root": root.path(),
    "steps": steps
      .iter()
      .zip(&previous_signatures)
      .map(|((store, _), previous)| json!({"path": stores[store], "account": ACCOUNT, "previous": previous}))
      .collect::<Vec<_>>(),
  });
  let python = python_reference(&request);

  let rust_steps: Vec<Value> = outcomes
    .iter()
    .map(|(signature, records)| {
      json!({"signature": signature, "records": records.iter().map(record_json).collect::<Vec<_>>()})
    })
    .collect();
  assert_eq!(rust_steps.len(), python["steps"].as_array().unwrap().len());
  for (index, rust_step) in rust_steps.iter().enumerate() {
    assert_eq!(rust_step, &python["steps"][index], "step {index} differs from the Python read");
  }
  assert_eq!(main_files(&stores), before, "a read changed a store's main file");

  // The branches the matrix exists for are the ones taken: an absent WAL reads 0:0, the WAL store reads its file, the
  // stored max floors the moved store, and the skipped steps return the stored signature.
  let parts = |index: usize| -> Vec<String> { outcomes[index].0.split(':').map(str::to_string).collect() };
  assert_eq!(parts(0)[2..4], ["0", "0"]);
  assert_ne!(parts(7)[2], "0");
  assert_eq!(outcomes[1], (outcomes[0].0.clone(), Vec::new()));
  assert_eq!(outcomes[3], (outcomes[2].0.clone(), Vec::new()));
  let moved_ids: Vec<&str> = outcomes[2].1.iter().map(|record| record.record_id.as_str()).collect();
  assert_eq!(moved_ids, ["opencode:boundary", "opencode:float-ms", "opencode:new-row"]);
  let records: usize = outcomes.iter().map(|(_, records)| records.len()).sum();
  let stored_rows = rows.len() + moved.len() + wal_rows.len();
  eprintln!(
    "opencode usage read matrix: {} stores, {stored_rows} rows, {} steps, {records} records",
    stores.len(),
    steps.len()
  );
}
