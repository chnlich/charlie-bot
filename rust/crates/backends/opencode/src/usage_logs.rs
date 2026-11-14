//! The opencode usage source's log side: the assistant rows of opencode's 2.0 SQLite database that carry token counts.
//!
//! Record id: `opencode:<message id>`, one per contributing row. Signature:
//! `<main size>:<main mtime_ns>:<wal size>:<wal mtime_ns>:<max time_updated>`. A stored signature whose file state still
//! matches skips the database. A moved database reads the rows at or above the stored max `time_updated`.

use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use anyhow::Context;
use charliebot_core::config::CharlieBotConfig;
use charliebot_core::paths;
use charliebot_core::text_repr::{is_truthy, value_str};
use charliebot_core::timefmt;
use charliebot_registry::usage::{
  QuotaAccount, RecordKind, SourceSweep, SweepScope, UsageOps, UsageRecord, UsageSourceImpl,
};
use regex::Regex;
use serde_json::{Map, Value};

use super::sqlite::open_read_only;
use super::usage_quota::{OpenCodeQuotaAccount, OpenCodeQuotaState};
use super::usage_sweep;
use charliebot_agents::usage_time::utc_isoformat;

/// The usage source's name: the ledger's source value and the usage page's card title.
pub const USAGE_SOURCE: &str = "opencode";

/// The one account of the opencode database (`_ACCOUNT`).
const ACCOUNT: &str = "default";

/// The row prefilter: a blob without a `tokens` key carries no token counts. The classes are ASCII, as SQLite's LIKE
/// folds case.
static TOKENS_KEY: LazyLock<Regex> =
  LazyLock::new(|| Regex::new(r#""[tT][oO][kK][eE][nN][sS]""#).expect("the tokens key pattern is valid"));

/// The `opencode` usage source: the store's one database, read in place and vacuumed by its sweep.
pub struct OpencodeLogs {
  db: PathBuf,
  proc_root: PathBuf,
  quota: Arc<OpenCodeQuotaAccount>,
}

impl OpencodeLogs {
  /// A source over `db`, with its shared quota account. `proc_root` is the process directory the sweep counts live writers in.
  pub fn new(db: PathBuf, proc_root: PathBuf, quota_state: Arc<OpenCodeQuotaState>) -> Self {
    let quota = Arc::new(OpenCodeQuotaAccount::new(db.clone(), quota_state));
    Self { db, proc_root, quota }
  }
}

impl UsageSourceImpl for OpencodeLogs {
  fn ops(&self) -> UsageOps {
    UsageOps { logs: true, read: true, live_cli_sessions: false, quota_accounts: true, sweep: true }
  }

  fn logs(&self, _cfg: &CharlieBotConfig) -> anyhow::Result<Vec<(PathBuf, String)>> {
    if self.db.exists() { Ok(vec![(self.db.clone(), ACCOUNT.to_string())]) } else { Ok(Vec::new()) }
  }

  fn read(&self, path: &Path, account: &str, previous: Option<&str>) -> anyhow::Result<(String, Vec<UsageRecord>)> {
    read(path, account, previous)
  }

  fn quota_accounts(&self, _cfg: &CharlieBotConfig) -> anyhow::Result<Vec<Arc<dyn QuotaAccount>>> {
    Ok(vec![Arc::clone(&self.quota) as Arc<dyn QuotaAccount>])
  }

  fn sweep(&self, scope: &SweepScope) -> anyhow::Result<SourceSweep> {
    usage_sweep::sweep(&self.db, &self.proc_root, scope)
  }
}

/// The records of the assistant rows the stored signature has not seen, and the database's new signature (`read`). The
/// database opens read-only, so a read never writes to it. A stored signature in another format reads the whole table.
pub fn read(path: &Path, _account: &str, previous: Option<&str>) -> anyhow::Result<(String, Vec<UsageRecord>)> {
  let state = file_state(path)?;
  let stored = stored_state(previous)?;
  if let (Some((stored_state, _)), Some(previous)) = (&stored, previous)
    && *stored_state == state
  {
    return Ok((previous.to_string(), Vec::new()));
  }
  let connection = open_read_only(path).with_context(|| format!("opening {} read-only", path.display()))?;
  let max_updated: i64 =
    connection.query_row("select coalesce(max(time_updated), 0) from session_message", [], |row| row.get(0))?;
  let floor = stored.map_or(0, |(_, floor)| floor);
  let mut statement = connection
    .prepare("select id, session_id, data from session_message where type = 'assistant' and time_updated >= ?")?;
  let mut records = Vec::new();
  // Each SQLite row is consumed while its records are built.
  let rows = statement
    .query_map([floor], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?)))?;
  for row in rows {
    let (message_id, session_id, data) = row?;
    if let Some(record) = record(&message_id, &session_id, &data)? {
      records.push(record);
    }
  }
  Ok((format!("{state}:{max_updated}"), records))
}

/// The file state of a store, `<main size>:<main mtime_ns>:<wal size>:<wal mtime_ns>`; an absent WAL file reads `0:0`
/// (`_file_state`).
fn file_state(path: &Path) -> anyhow::Result<String> {
  let main = fs::metadata(path).with_context(|| format!("statting {}", path.display()))?;
  let mut wal_path = path.as_os_str().to_owned();
  wal_path.push("-wal");
  let wal_path = PathBuf::from(wal_path);
  let (wal_size, wal_mtime) = match fs::metadata(&wal_path) {
    Ok(wal) => (wal.len(), timefmt::file_time_ns(wal.mtime(), wal.mtime_nsec())),
    Err(error) if error.kind() == io::ErrorKind::NotFound => (0, 0),
    Err(error) => return Err(error).with_context(|| format!("statting {}", wal_path.display())),
  };
  let (main_size, main_mtime) = (main.len(), timefmt::file_time_ns(main.mtime(), main.mtime_nsec()));
  Ok(format!("{main_size}:{main_mtime}:{wal_size}:{wal_mtime}"))
}

/// The file state and max `time_updated` of a stored signature in the current format, or `None` for any other format
/// (`_stored_state`).
fn stored_state(previous: Option<&str>) -> anyhow::Result<Option<(String, i64)>> {
  let Some(previous) = previous else {
    return Ok(None);
  };
  let parts: Vec<&str> = previous.split(':').collect();
  let numeric = |part: &&str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
  if parts.len() != 5 || !parts.iter().all(numeric) {
    return Ok(None);
  }
  let floor = parts[4]
    .parse::<i64>()
    .with_context(|| format!("stored signature {previous:?} has a time_updated out of range"))?;
  Ok(Some((parts[..4].join(":"), floor)))
}

/// The ledger record of one assistant row, or `None` when the row contributes nothing (`_record`).
fn record(message_id: &str, session_id: &str, data: &str) -> anyhow::Result<Option<UsageRecord>> {
  if !TOKENS_KEY.is_match(data) {
    return Ok(None);
  }
  // A blob that is not strict JSON, or nests past the parser's limit, contributes nothing. A blob that is valid JSON
  // but not an object contributes nothing too.
  let Ok(Value::Object(blob)) = serde_json::from_str::<Value>(data) else {
    return Ok(None);
  };
  let tokens = blob.get("tokens").and_then(Value::as_object);
  let cache = field(tokens, "cache").and_then(Value::as_object);
  let in_fresh = field(tokens, "input");
  let output = field(tokens, "output");
  if !(in_fresh.is_some_and(is_truthy) || output.is_some_and(is_truthy)) {
    return Ok(None);
  }

  let model_blob = blob.get("model").and_then(Value::as_object);
  let provider = field(model_blob, "providerID");
  let mut model = text_or(field(model_blob, "id"), "unknown")?;
  if model.starts_with('/') {
    model = format!("{} ({})", paths::name(&model), provider.map_or_else(|| "None".to_string(), value_str));
  }
  let account = text_or(provider, "unknown")?;

  let created = field(blob.get("time").and_then(Value::as_object), "created");
  let ts = match created {
    Some(Value::Number(number)) => {
      utc_isoformat(number.as_f64().context("time.created is not a finite number")? / 1000.0)?
    }
    Some(Value::Bool(flag)) => utc_isoformat(f64::from(u8::from(*flag)) / 1000.0)?,
    _ => String::new(),
  };

  Ok(Some(UsageRecord {
    record_id: format!("opencode:{message_id}"),
    kind: RecordKind::Native,
    source: USAGE_SOURCE.to_string(),
    model,
    account,
    ts,
    in_fresh: count(in_fresh)?,
    cache_write: count(field(cache, "write"))?,
    cache_read: count(field(cache, "read"))?,
    output: count(output)?,
    in_unsplit: 0,
    sessions: vec![session_id.to_string()],
  }))
}

/// A key of a JSON object, or `None` when the object is absent or lacks the key.
fn field<'a>(object: Option<&'a Map<String, Value>>, key: &str) -> Option<&'a Value> {
  object?.get(key)
}

/// A text field: a truthy value must be a string, and a falsy or absent one reads as `default`.
fn text_or(value: Option<&Value>, default: &str) -> anyhow::Result<String> {
  match value {
    Some(value) if is_truthy(value) => {
      Ok(value.as_str().with_context(|| format!("{value} is not a string"))?.to_string())
    }
    _ => Ok(default.to_string()),
  }
}

/// A token count: a truthy value must be a JSON integer, and a falsy or absent one reads as 0.
fn count(value: Option<&Value>) -> anyhow::Result<i64> {
  match value {
    Some(value) if is_truthy(value) => value.as_i64().with_context(|| format!("token count {value} is not an integer")),
    _ => Ok(0),
  }
}

#[cfg(test)]
mod tests {
  use std::collections::BTreeSet;

  use charliebot_registry::Registry;
  use serde_json::json;
  use tempfile::TempDir;

  use super::*;
  use crate::test_store::{Message, clear_messages, create_store, touch, write_message};

  /// A created time: 2026-10-14T17:46:40 UTC.
  const CREATED_MS: i64 = 1_792_000_000_000;

  fn blob(model: &str, provider: &str, input: i64, output: i64) -> String {
    json!({
      "time": {"created": CREATED_MS},
      "model": {"id": model, "providerID": provider},
      "tokens": {"input": input, "output": output, "reasoning": 0, "cache": {"read": 5, "write": 7}},
    })
    .to_string()
  }

  fn assistant<'a>(id: &'a str, updated: i64, data: &'a str) -> Message<'a> {
    Message { id, session: "sess-a", role: "assistant", updated, data }
  }

  fn ids(records: &[UsageRecord]) -> Vec<&str> {
    records.iter().map(|record| record.record_id.as_str()).collect()
  }

  fn touch_path(db: &Path, step: u64) {
    touch(db, 1_900_000_000 + step);
  }

  #[test]
  fn logs_names_the_database_only_when_it_exists() {
    // A host without the database contributes nothing.
    let root = TempDir::new().unwrap();
    let db = root.path().join("opencode.db");
    let cfg = Registry::new().load_config(root.path()).unwrap();
    let source = OpencodeLogs::new(db.clone(), root.path().join("proc"), Arc::default());

    assert!(source.logs(&cfg).unwrap().is_empty());
    create_store(&db, &[]);
    assert_eq!(source.logs(&cfg).unwrap(), vec![(db, ACCOUNT.to_string())]);
  }

  #[test]
  fn opencode_capture_rows() {
    // Every assistant row with tokens becomes one record. A zero-token row, an error row without a tokens key and a
    // user row contribute nothing.
    let root = TempDir::new().unwrap();
    let db = root.path().join("opencode.db");
    let zero = json!({"tokens": {"input": 0, "output": 0}}).to_string();
    let error = json!({"error": "boom", "model": {"id": "model-a", "providerID": "provider-a"}}).to_string();
    let user = blob("model-a", "provider-a", 70, 7);
    create_store(
      &db,
      &[
        assistant("m1", 100, &blob("model-a", "provider-a", 100, 30)),
        assistant("m2", 200, &blob("model-b", "provider-b", 50, 5)),
        assistant("m3", 300, &zero),
        assistant("m4", 400, &error),
        Message { id: "m5", session: "sess-a", role: "user", updated: 500, data: &user },
      ],
    );

    let (signature, records) = read(&db, ACCOUNT, None).unwrap();

    assert_eq!(ids(&records), ["opencode:m1", "opencode:m2"]);
    assert_eq!(
      records[0],
      UsageRecord {
        record_id: "opencode:m1".to_string(),
        kind: RecordKind::Native,
        source: "opencode".to_string(),
        model: "model-a".to_string(),
        account: "provider-a".to_string(),
        ts: "2026-10-14T17:46:40+00:00".to_string(),
        in_fresh: 100,
        cache_write: 7,
        cache_read: 5,
        output: 30,
        in_unsplit: 0,
        sessions: vec!["sess-a".to_string()],
      }
    );
    assert_eq!(signature.split(':').count(), 5);
  }

  #[test]
  fn capture_rereads_an_updated_row() {
    // Each read passes the signature of the one before, so a row moved to a higher time_updated is read again alone.
    let root = TempDir::new().unwrap();
    let db = root.path().join("opencode.db");
    create_store(&db, &[assistant("m1", 100, &blob("model-a", "provider-a", 100, 10))]);
    let (first, records) = read(&db, ACCOUNT, None).unwrap();
    assert_eq!(ids(&records), ["opencode:m1"]);

    write_message(&db, &assistant("m1", 300, &blob("model-a", "provider-a", 222, 22)));
    touch_path(&db, 1);
    let (second, records) = read(&db, ACCOUNT, Some(&first)).unwrap();
    assert_eq!(ids(&records), ["opencode:m1"]);
    assert_eq!((records[0].in_fresh, records[0].output), (222, 22));
    assert_ne!(second, first);
  }

  #[test]
  fn capture_after_source_row_deletion_returns_no_records() {
    let root = TempDir::new().unwrap();
    let db = root.path().join("opencode.db");
    create_store(&db, &[assistant("m1", 100, &blob("model-a", "provider-a", 100, 10))]);
    let (signature, records) = read(&db, ACCOUNT, None).unwrap();
    assert_eq!(ids(&records), ["opencode:m1"]);

    clear_messages(&db);
    touch_path(&db, 1);
    let (_, records) = read(&db, ACCOUNT, Some(&signature)).unwrap();
    assert!(records.is_empty());
  }

  #[test]
  fn capture_skips_an_unchanged_db_and_never_writes_to_it() {
    // A database whose files sit still since the stored signature yields nothing, and the read leaves its bytes, mtime
    // and directory untouched.
    let root = TempDir::new().unwrap();
    let db = root.path().join("opencode.db");
    create_store(&db, &[assistant("m1", 100, &blob("model-a", "provider-a", 100, 10))]);
    let (signature, records) = read(&db, ACCOUNT, None).unwrap();
    assert_eq!(records.len(), 1);
    let names = dir_names(root.path());
    let bytes = fs::read(&db).unwrap();
    let mtime = fs::metadata(&db).unwrap().modified().unwrap();

    let (again, records) = read(&db, ACCOUNT, Some(&signature)).unwrap();

    assert_eq!(again, signature);
    assert!(records.is_empty());
    assert_eq!(fs::read(&db).unwrap(), bytes);
    assert_eq!(fs::metadata(&db).unwrap().modified().unwrap(), mtime);
    assert_eq!(dir_names(root.path()), names);
  }

  #[test]
  fn old_format_signature_forces_one_reread() {
    // A stored signature in any other format reads the whole table once, and the new signature is in the current format
    // and then skips.
    let root = TempDir::new().unwrap();
    let db = root.path().join("opencode.db");
    create_store(
      &db,
      &[
        assistant("m1", 100, &blob("model-a", "provider-a", 100, 10)),
        assistant("m2", 200, &blob("model-a", "provider-a", 200, 20)),
      ],
    );
    let (_, records) = read(&db, ACCOUNT, None).unwrap();
    assert_eq!(records.len(), 2);

    let (signature, records) = read(&db, ACCOUNT, Some("legacy-gate-signature")).unwrap();
    assert_eq!(records.len(), 2, "the whole table, not just the rows past the stored max");
    assert_eq!(signature.split(':').count(), 5);
    let (_, records) = read(&db, ACCOUNT, Some(&signature)).unwrap();
    assert!(records.is_empty());
  }

  fn dir_names(dir: &Path) -> BTreeSet<String> {
    fs::read_dir(dir).unwrap().map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned()).collect()
  }
}
