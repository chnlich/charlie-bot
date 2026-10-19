use std::collections::BTreeMap;

use charliebot_registry::usage::{RecordKind, UsageRecord};
use rusqlite::{Connection, params};

use super::*;

const SOURCE: &str = "src-a";
const HOST: &str = "host-a";
const TS_A: &str = "2026-01-10T08:00:00+00:00";
const TS_B: &str = "2026-01-11T09:30:00+00:00";

fn record(record_id: &str, kind: RecordKind, sessions: &[&str]) -> UsageRecord {
  UsageRecord {
    record_id: record_id.to_string(),
    kind,
    source: SOURCE.to_string(),
    model: "model-a".to_string(),
    account: "acct-a".to_string(),
    ts: TS_A.to_string(),
    in_fresh: 10,
    cache_write: 2,
    cache_read: 3,
    in_unsplit: 0,
    output: 5,
    sessions: sessions.iter().map(|session| (*session).to_string()).collect(),
  }
}

fn table_rows(ledger: &UsageLedger) -> Vec<LedgerRow> {
  rows_from_accs(ledger.grouped_accs(sql::MODEL_ROWS_SQL).unwrap()).0
}

#[test]
fn record_ids_are_deduplicated_across_hosts_and_capture_signatures_replace() {
  let dir = tempfile::tempdir().unwrap();
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  let rec = record("rec-1", RecordKind::Native, &["sess-a"]);
  assert_eq!(ledger.record_file("host-a", "/logs/a.jsonl", "sig-a", std::slice::from_ref(&rec)).unwrap(), 1);
  assert_eq!(ledger.record_file("host-b", "/logs/b.jsonl", "sig-b", std::slice::from_ref(&rec)).unwrap(), 1);
  let rows = ledger.model_rows().unwrap();
  assert_eq!((rows[0].calls, rows[0].output, rows[0].total), (1, 5, 20));
  ledger.record_file("host-a", "/logs/a.jsonl", "sig-new", &[]).unwrap();
  assert_eq!(ledger.captured_sigs("host-a").unwrap(), BTreeMap::from([("/logs/a.jsonl".into(), "sig-new".into())]));
}

#[test]
fn fallback_rows_retire_on_any_native_session_and_reupserts_keep_old_native_sessions() {
  let dir = tempfile::tempdir().unwrap();
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  let mut fallback = record("rec-fb", RecordKind::Fallback, &["sess-a", "sess-b"]);
  fallback.model = "model-fb".into();
  fallback.ts = TS_B.into();
  ledger.record_file(HOST, "/logs/fb.jsonl", "fb", &[fallback]).unwrap();
  assert_eq!(ledger.model_rows().unwrap()[0].fallback_calls, 1);
  ledger
    .record_file(HOST, "/logs/native-a.jsonl", "na", &[record("rec-native", RecordKind::Native, &["sess-a"])])
    .unwrap();
  assert!(ledger.model_rows().unwrap().iter().all(|row| row.model != "model-fb"));

  ledger
    .record_file(HOST, "/logs/native-b.jsonl", "nb", &[record("rec-native", RecordKind::Native, &["sess-b"])])
    .unwrap();
  let sessions = ledger
    .conn
    .prepare("SELECT session FROM native_sessions ORDER BY session")
    .unwrap()
    .query_map([], |row| row.get::<_, String>(0))
    .unwrap()
    .collect::<rusqlite::Result<Vec<_>>>()
    .unwrap();
  assert_eq!(sessions, ["sess-a", "sess-b"]);
}

#[test]
fn model_rows_sort_accounts_and_spans_while_native_starts_ignore_fallbacks_and_empty_times() {
  let dir = tempfile::tempdir().unwrap();
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  let mut fallback = record("rec-fb", RecordKind::Fallback, &["sess-f"]);
  fallback.account = "acct-b".into();
  fallback.ts = TS_A.into();
  fallback.output = 7;
  let mut native = record("rec-native", RecordKind::Native, &["sess-n"]);
  native.ts = TS_B.into();
  let mut undated = record("rec-undated", RecordKind::Native, &["sess-u"]);
  undated.ts.clear();
  ledger.record_file(HOST, "/logs/a.jsonl", "sig", &[fallback, native, undated]).unwrap();
  let (rows, starts) = ledger.model_rows_with_native_starts().unwrap();
  assert_eq!(starts, BTreeMap::from([(SOURCE.into(), "2026-01-11".into())]));
  assert_eq!((rows[0].first.as_str(), rows[0].last.as_str(), rows[0].calls), ("2026-01-10", "2026-01-11", 3));
  assert_eq!(rows[0].fallback_calls, 1);
  assert_eq!(rows[0].fallback_output, 7);
  assert_eq!(rows[0].accounts.iter().map(|account| account.name.as_str()).collect::<Vec<_>>(), ["acct-a", "acct-b"]);
}

#[test]
fn ts_conflicts_keep_the_earlier_nonempty_value_and_unsplit_counts_on_all_reads() {
  let dir = tempfile::tempdir().unwrap();
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  let mut first = record("rec-1", RecordKind::Native, &["sess-a"]);
  first.in_unsplit = 7;
  ledger.record_file(HOST, "/logs/a.jsonl", "s1", &[first.clone()]).unwrap();
  first.ts = TS_B.into();
  ledger.record_file(HOST, "/logs/a.jsonl", "s2", &[first.clone()]).unwrap();
  let stored: String =
    ledger.conn.query_row("SELECT ts FROM usage WHERE record_id = 'rec-1'", [], |row| row.get(0)).unwrap();
  assert_eq!(stored, TS_A);
  ledger.model_rows().unwrap();
  let served = ledger.model_rows().unwrap();
  assert_eq!((served[0].in_unsplit, served[0].total), (7, 27));
  assert_eq!(table_rows(&ledger), served);
}

#[test]
fn aggregate_tracks_same_day_rewrites_cross_day_moves_and_session_retirement() {
  let dir = tempfile::tempdir().unwrap();
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  let mut first = record("rec-1", RecordKind::Native, &["sess-a"]);
  first.ts = TS_B.into();
  ledger.record_file(HOST, "/logs/a.jsonl", "s1", &[first]).unwrap();
  ledger.model_rows().unwrap();
  let mut moved = record("rec-1", RecordKind::Native, &["sess-a"]);
  moved.ts = TS_A.into();
  moved.output = 9;
  let mut same_day = record("rec-2", RecordKind::Native, &["sess-b"]);
  same_day.ts = TS_B.into();
  same_day.output = 6;
  ledger.record_file(HOST, "/logs/a.jsonl", "s2", &[moved, same_day]).unwrap();

  let mut fallback = record("rec-fb", RecordKind::Fallback, &["sess-x"]);
  fallback.model = "model-fb".into();
  fallback.ts = TS_B.into();
  ledger.record_file(HOST, "/logs/fb.jsonl", "fb", &[fallback]).unwrap();
  ledger.model_rows().unwrap();
  let mut native = record("rec-native", RecordKind::Native, &["sess-x"]);
  native.model = "model-native".into();
  ledger.record_file(HOST, "/logs/native.jsonl", "native", &[native]).unwrap();
  assert_eq!(ledger.model_rows().unwrap(), table_rows(&ledger));
  assert_eq!(ledger.model_rows().unwrap().iter().find(|row| row.model == "model-a").unwrap().calls, 2);
  assert!(ledger.model_rows().unwrap().iter().all(|row| row.model != "model-fb"));
}

#[test]
fn batch_release_commits_finished_files_after_a_later_file_fails_atomically() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("ledger.sqlite3");
  let mut writer = UsageLedger::open(&path).unwrap();
  let reader = UsageLedger::open(&path).unwrap();
  writer
    .conn
    .execute_batch(
      "CREATE TRIGGER reject_usage BEFORE INSERT ON usage WHEN new.record_id = 'rec-bad' BEGIN SELECT RAISE(ABORT, 'fixture rejection'); END;",
    )
    .unwrap();
  let result = writer.batch(|ledger| {
    ledger.record_file(HOST, "/logs/good.jsonl", "good", &[record("rec-good", RecordKind::Native, &["sess-good"])])?;
    assert!(reader.captured_sigs(HOST)?.is_empty());
    ledger.record_file(HOST, "/logs/bad.jsonl", "bad", &[record("rec-bad", RecordKind::Native, &["sess-bad"])])?;
    Ok(())
  });
  assert!(result.is_err());
  assert_eq!(reader.captured_sigs(HOST).unwrap(), BTreeMap::from([("/logs/good.jsonl".into(), "good".into())]));
  let session_rows: i64 = reader
    .conn
    .query_row("SELECT COUNT(*) FROM native_sessions WHERE session = 'sess-bad'", [], |row| row.get(0))
    .unwrap();
  assert_eq!(session_rows, 0);
  let usage_rows: i64 =
    reader.conn.query_row("SELECT COUNT(*) FROM usage WHERE record_id = 'rec-bad'", [], |row| row.get(0)).unwrap();
  assert_eq!(usage_rows, 0);
}

#[test]
fn unknown_stored_kind_fails_the_grouped_read() {
  let dir = tempfile::tempdir().unwrap();
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  ledger
    .conn
    .execute(
      "INSERT INTO usage (record_id, kind, source, model, account, host, ts, in_fresh, cache_write, cache_read, in_unsplit, output, origin, captured_at) VALUES ('rec-x', 'replayed', ?, 'model-x', 'acct-a', ?, ?, 1, 0, 0, 0, 2, '/logs/x.jsonl', ?)",
      params![SOURCE, HOST, TS_A, TS_A],
    )
    .unwrap();
  assert!(ledger.model_rows().is_err());
}

#[test]
fn open_swaps_five_changed_sum_triggers_and_reprices_aggregate() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("ledger.sqlite3");
  {
    let mut ledger = UsageLedger::open(&path).unwrap();
    ledger.record_file(HOST, "/logs/a.jsonl", "sig", &[record("rec-1", RecordKind::Native, &["sess-a"])]).unwrap();
    ledger.model_rows().unwrap();
  }
  {
    let raw = Connection::open(&path).unwrap();
    for name in sql::AGG_SUM_TRIGGER_NAMES {
      raw
        .execute_batch(&format!(
          "DROP TRIGGER {name}; CREATE TRIGGER {name} AFTER INSERT ON usage BEGIN SELECT 1; END;"
        ))
        .unwrap();
    }
    raw.execute_batch("DELETE FROM usage_agg; INSERT INTO usage_agg VALUES ('wrong', 'wrong', 'wrong', 'native', '2020-01-01', 99, 99, 0, 0, 0, 99);")
      .unwrap();
  }
  let ledger = UsageLedger::open(&path).unwrap();
  let mut expected = sql::agg_trigger_statements()
    .iter()
    .take(5)
    .map(|statement| stored_trigger_sql(statement).unwrap())
    .collect::<Vec<_>>();
  expected.sort();
  let mut actual = ledger
    .conn
    .prepare("SELECT sql FROM sqlite_master WHERE type = 'trigger' AND name <> 'usage_agg_purge_empty'")
    .unwrap()
    .query_map([], |row| row.get::<_, String>(0))
    .unwrap()
    .collect::<rusqlite::Result<Vec<_>>>()
    .unwrap();
  actual.sort();
  assert_eq!(actual, expected);
  let agg: (i64, i64) =
    ledger.conn.query_row("SELECT calls, output FROM usage_agg", [], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
  assert_eq!(agg, (1, 5));
}

#[test]
fn last_capture_time_is_host_scoped_and_default_path_is_under_home_usage() {
  let dir = tempfile::tempdir().unwrap();
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  assert!(ledger.last_capture_at(HOST).unwrap().is_none());
  ledger.mark_capture_finished(HOST).unwrap();
  let stamped = ledger.last_capture_at(HOST).unwrap().unwrap();
  assert_eq!(stamped.offset(), time::UtcOffset::UTC);
  assert!(ledger.last_capture_at("host-b").unwrap().is_none());
  assert_eq!(default_ledger_path(Path::new("/home/fixture")), Path::new("/home/fixture/usage/ledger.sqlite3"));
}

#[test]
fn timestamps_use_python_utc_isoformat_microsecond_precision() {
  let instant = OffsetDateTime::from_unix_timestamp(0).unwrap();
  assert_eq!(python_utc_isoformat(instant).unwrap(), "1970-01-01T00:00:00+00:00");
  let with_micros = instant.replace_nanosecond(123_456_000).unwrap();
  assert_eq!(python_utc_isoformat(with_micros).unwrap(), "1970-01-01T00:00:00.123456+00:00");
}
