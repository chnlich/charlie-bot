use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use charliebot_registry::usage::{RecordKind, UsageRecord};
use rusqlite::{Connection, params, params_from_iter};

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
  rows_from_accs(ledger.all_time_accs(false).unwrap()).unwrap().0
}

#[test]
fn record_ids_are_deduplicated_across_hosts_and_capture_signatures_replace() {
  let dir = tempfile::tempdir().unwrap();
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  let rec = record("rec-1", RecordKind::Native, &["sess-a"]);
  assert_eq!(ledger.record_file("host-a", "/logs/a.jsonl", "sig-a", std::slice::from_ref(&rec)).unwrap(), 1);
  assert_eq!(ledger.record_file("host-b", "/logs/b.jsonl", "sig-b", std::slice::from_ref(&rec)).unwrap(), 1);
  let rows = ledger.model_rows().unwrap();
  assert_eq!((rows[0].calls, rows[0].output.int(), rows[0].total.int()), (1, 5, 20));
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
  assert_eq!(rows[0].fallback_output.int(), 7);
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
  assert_eq!((served[0].in_unsplit.int(), served[0].total.int()), (7, 27));
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
fn timestamps_use_utc_isoformat_microsecond_precision() {
  let instant = OffsetDateTime::from_unix_timestamp(0).unwrap();
  assert_eq!(utc_isoformat_text(instant).unwrap(), "1970-01-01T00:00:00+00:00");
  let with_micros = instant.replace_nanosecond(123_456_000).unwrap();
  assert_eq!(utc_isoformat_text(with_micros).unwrap(), "1970-01-01T00:00:00.123456+00:00");
}

// The cases of tests/test_usage_ledger.py, in the file's order.

fn models(rows: &[LedgerRow]) -> Vec<&str> {
  rows.iter().map(|row| row.model.as_str()).collect()
}

fn agg_rows(ledger: &mut UsageLedger) -> Vec<LedgerRow> {
  assert!(ledger.agg_ready().unwrap(), "the aggregate has been backfilled and serves");
  rows_from_accs(ledger.all_time_accs(true).unwrap()).unwrap().0
}

fn assert_served_on_the_table(ledger: &mut UsageLedger) {
  let table = table_rows(ledger);
  assert_eq!(ledger.model_rows().unwrap(), table);
  assert_eq!(agg_rows(ledger), table);
}

fn column_text(conn: &Connection, query: &str) -> Vec<String> {
  let mut statement = conn.prepare(query).unwrap();
  let rows = statement.query_map([], |row| row.get::<_, String>(0)).unwrap();
  rows.collect::<rusqlite::Result<Vec<_>>>().unwrap()
}

fn sql_rows(conn: &Connection, query: &str) -> Vec<Vec<rusqlite::types::Value>> {
  let mut statement = conn.prepare(query).unwrap();
  let width = statement.column_count();
  let rows = statement.query_map([], |row| {
    (0..width).map(|index| row.get::<_, rusqlite::types::Value>(index)).collect::<rusqlite::Result<Vec<_>>>()
  });
  rows.unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap()
}

/// Every usage, aggregate and meta row, in a stable order.
fn snapshot(conn: &Connection) -> [Vec<Vec<rusqlite::types::Value>>; 3] {
  [
    sql_rows(conn, "SELECT * FROM usage ORDER BY record_id"),
    sql_rows(conn, "SELECT * FROM usage_agg ORDER BY source, model, account, kind, day"),
    sql_rows(conn, "SELECT * FROM ledger_meta ORDER BY key"),
  ]
}

fn trigger_sqls(conn: &Connection) -> BTreeMap<String, String> {
  let mut statement = conn.prepare("SELECT name, sql FROM sqlite_master WHERE type = 'trigger'").unwrap();
  let rows = statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))).unwrap();
  rows.collect::<rusqlite::Result<BTreeMap<_, _>>>().unwrap()
}

/// One record written through raw SQL the way a release's record_file lands it: sessions first, then the usage row.
fn write_raw_usage(conn: &Connection, rec: &UsageRecord) {
  for session in &rec.sessions {
    conn.execute("INSERT INTO native_sessions (session, source) VALUES (?, ?)", params![session, rec.source]).unwrap();
  }
  conn
    .execute(
      "INSERT INTO usage (record_id, kind, source, model, account, host, ts, in_fresh, cache_write, cache_read, in_unsplit, output, origin, captured_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, '/x', ?)",
      params![
        rec.record_id,
        kind_value(rec.kind),
        rec.source,
        rec.model,
        rec.account,
        HOST,
        rec.ts,
        rec.in_fresh,
        rec.cache_write,
        rec.cache_read,
        rec.in_unsplit,
        rec.output,
        rec.ts,
      ],
    )
    .unwrap();
}

/// The aggregate triggers the pre-fix release shipped: its add arm skipped any (group, day) row that already existed.
/// Only these two bodies differ from the module's; the three subtract-only bodies are unchanged.
const PRE_FIX_INSERT_TRIGGER: &str = r#"
CREATE TRIGGER usage_agg_after_insert AFTER INSERT ON usage
BEGIN
  INSERT INTO usage_agg (source, model, account, kind, day, calls, in_fresh, cache_write, cache_read, in_unsplit, output)
  SELECT new.source, new.model, new.account, new.kind, SUBSTR(new.ts, 1, 10), 1,
         new.in_fresh, new.cache_write, new.cache_read, new.in_unsplit, new.output
  WHERE NOT (new.kind = 'fallback' AND EXISTS (
      SELECT 1 FROM fallback_sessions fs JOIN native_sessions ns ON ns.session = fs.session
      WHERE fs.record_id = new.record_id))
    AND NOT EXISTS (
      SELECT 1 FROM usage_agg a WHERE a.source = new.source AND a.model = new.model
      AND a.account = new.account AND a.kind = new.kind AND a.day = SUBSTR(new.ts, 1, 10))
  ON CONFLICT(source, model, account, kind, day) DO UPDATE SET
    calls = calls + excluded.calls, in_fresh = in_fresh + excluded.in_fresh,
    cache_write = cache_write + excluded.cache_write, cache_read = cache_read + excluded.cache_read,
    in_unsplit = in_unsplit + excluded.in_unsplit, output = output + excluded.output;
END;"#;

const PRE_FIX_UPDATE_TRIGGER: &str = r#"
CREATE TRIGGER usage_agg_after_update AFTER UPDATE ON usage
WHEN old.kind <> new.kind OR old.source <> new.source OR old.model <> new.model
  OR old.account <> new.account OR old.ts <> new.ts OR old.in_fresh <> new.in_fresh
  OR old.cache_write <> new.cache_write OR old.cache_read <> new.cache_read
  OR old.in_unsplit <> new.in_unsplit OR old.output <> new.output
BEGIN
  INSERT INTO usage_agg (source, model, account, kind, day, calls, in_fresh, cache_write, cache_read, in_unsplit, output)
  SELECT old.source, old.model, old.account, old.kind, SUBSTR(old.ts, 1, 10), -1,
         -old.in_fresh, -old.cache_write, -old.cache_read, -old.in_unsplit, -old.output
  WHERE NOT (old.kind = 'fallback' AND EXISTS (
      SELECT 1 FROM fallback_sessions fs JOIN native_sessions ns ON ns.session = fs.session
      WHERE fs.record_id = old.record_id))
    AND EXISTS (
      SELECT 1 FROM usage_agg a WHERE a.source = old.source AND a.model = old.model
      AND a.account = old.account AND a.kind = old.kind AND a.day = SUBSTR(old.ts, 1, 10));
  INSERT INTO usage_agg (source, model, account, kind, day, calls, in_fresh, cache_write, cache_read, in_unsplit, output)
  SELECT new.source, new.model, new.account, new.kind, SUBSTR(new.ts, 1, 10), 1,
         new.in_fresh, new.cache_write, new.cache_read, new.in_unsplit, new.output
  WHERE NOT (new.kind = 'fallback' AND EXISTS (
      SELECT 1 FROM fallback_sessions fs JOIN native_sessions ns ON ns.session = fs.session
      WHERE fs.record_id = new.record_id))
    AND NOT EXISTS (
      SELECT 1 FROM usage_agg a WHERE a.source = new.source AND a.model = new.model
      AND a.account = new.account AND a.kind = new.kind AND a.day = SUBSTR(new.ts, 1, 10))
  ON CONFLICT(source, model, account, kind, day) DO UPDATE SET
    calls = calls + excluded.calls, in_fresh = in_fresh + excluded.in_fresh,
    cache_write = cache_write + excluded.cache_write, cache_read = cache_read + excluded.cache_read,
    in_unsplit = in_unsplit + excluded.in_unsplit, output = output + excluded.output;
END;"#;

/// A ledger the pre-fix release left: the schema, the pre-fix trigger bodies, `served` written and the aggregate
/// backfilled over them, then `late` written under the same bodies, whose adds a served group-day drops.
fn pre_fix_ledger(path: &Path, served: &[UsageRecord], late: &[UsageRecord]) {
  let raw = Connection::open(path).unwrap();
  raw.execute_batch(sql::SCHEMA).unwrap();
  raw.execute_batch(&format!("{PRE_FIX_INSERT_TRIGGER}{PRE_FIX_UPDATE_TRIGGER}")).unwrap();
  for rec in served.iter().chain(late) {
    write_raw_usage(&raw, rec);
  }
  let placeholders = vec!["?"; served.len()].join(",");
  raw
    .execute(
      &format!(
        "INSERT INTO usage_agg (source, model, account, kind, day, calls, in_fresh, cache_write, cache_read, in_unsplit, output)
         SELECT source, model, account, kind, SUBSTR(ts, 1, 10), COUNT(*),
         SUM(in_fresh), SUM(cache_write), SUM(cache_read), SUM(in_unsplit), SUM(output)
         FROM usage WHERE record_id IN ({placeholders})
         GROUP BY source, model, account, kind, SUBSTR(ts, 1, 10)
         ON CONFLICT(source, model, account, kind, day) DO UPDATE SET
         calls = excluded.calls, in_fresh = excluded.in_fresh, cache_write = excluded.cache_write,
         cache_read = excluded.cache_read, in_unsplit = excluded.in_unsplit, output = excluded.output"
      ),
      params_from_iter(served.iter().map(|rec| rec.record_id.as_str())),
    )
    .unwrap();
  raw.execute_batch("INSERT INTO ledger_meta (key, value) VALUES ('agg_backfilled', '1'), ('schema', '2')").unwrap();
}

/// Makes the usage insert fail for `rec-bad`, after its sessions have registered.
const REJECT_BAD_USAGE: &str = "CREATE TRIGGER reject_usage BEFORE INSERT ON usage WHEN new.record_id = 'rec-bad' BEGIN SELECT RAISE(ABORT, 'fixture rejection'); END;";

#[test]
fn one_record_id_from_two_hosts_counts_once() {
  let dir = tempfile::tempdir().unwrap();
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  let rec = record("rec-1", RecordKind::Native, &["sess-a"]);
  assert_eq!(ledger.record_file("host-a", "/logs/a.jsonl", "sig-a", std::slice::from_ref(&rec)).unwrap(), 1);
  assert_eq!(ledger.record_file("host-b", "/logs/b.jsonl", "sig-b", std::slice::from_ref(&rec)).unwrap(), 1);
  let rows = ledger.model_rows().unwrap();
  assert_eq!(rows.len(), 1);
  assert_eq!((rows[0].calls, rows[0].output.int(), rows[0].total.int()), (1, 5, 20));
}

#[test]
fn fallback_excluded_once_any_session_is_native() {
  let dir = tempfile::tempdir().unwrap();
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  let mut fallback = record("rec-fb", RecordKind::Fallback, &["sess-a", "sess-b"]);
  fallback.model = "model-fb".into();
  fallback.ts = TS_B.into();
  ledger.record_file(HOST, "/logs/fb.jsonl", "sig-fb", &[fallback]).unwrap();
  let rows = ledger.model_rows().unwrap();
  assert_eq!(models(&rows), ["model-fb"]);
  assert_eq!((rows[0].fallback_calls, rows[0].fallback_output.int()), (1, 5));
  let mut native = record("rec-native", RecordKind::Native, &["sess-a"]);
  native.model = "model-native".into();
  ledger.record_file(HOST, "/logs/native.jsonl", "sig-native", &[native]).unwrap();
  let rows = ledger.model_rows().unwrap();
  assert_eq!(models(&rows), ["model-native"]);
}

#[test]
fn rows_equal_for_either_write_order() {
  let dir = tempfile::tempdir().unwrap();
  let native = record("rec-native", RecordKind::Native, &["sess-a"]);
  let mut fallback = record("rec-fb", RecordKind::Fallback, &["sess-b"]);
  fallback.account = "acct-b".into();
  fallback.ts = TS_B.into();
  fallback.output = 7;
  let native_first = {
    let mut ledger = UsageLedger::open(dir.path().join("native-first.sqlite3")).unwrap();
    ledger.record_file(HOST, "/logs/native.jsonl", "sig-n", std::slice::from_ref(&native)).unwrap();
    ledger.record_file(HOST, "/logs/fb.jsonl", "sig-f", std::slice::from_ref(&fallback)).unwrap();
    ledger.model_rows().unwrap()
  };
  let fallback_first = {
    let mut ledger = UsageLedger::open(dir.path().join("fallback-first.sqlite3")).unwrap();
    ledger.record_file(HOST, "/logs/fb.jsonl", "sig-f", std::slice::from_ref(&fallback)).unwrap();
    ledger.record_file(HOST, "/logs/native.jsonl", "sig-n", std::slice::from_ref(&native)).unwrap();
    ledger.model_rows().unwrap()
  };
  assert_eq!(native_first, fallback_first);
  assert_eq!(native_first.len(), 1);
  let row = &native_first[0];
  assert_eq!((row.calls, row.total.int()), (2, 42));
  assert_eq!((row.first.as_str(), row.last.as_str()), ("2026-01-10", "2026-01-11"));
  assert_eq!((row.fallback_calls, row.fallback_output.int()), (1, 7));
  let accounts = row.accounts.iter().map(|account| (account.name.as_str(), account.total.int())).collect::<Vec<_>>();
  assert_eq!(accounts, [("acct-b", 22), ("acct-a", 20)]);
}

#[test]
fn empty_ts_never_becomes_first_or_last() {
  let dir = tempfile::tempdir().unwrap();
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  let mut empty = record("rec-empty", RecordKind::Native, &["sess-e"]);
  empty.ts.clear();
  let mut early = record("rec-early", RecordKind::Native, &["sess-a"]);
  early.account = "acct-b".into();
  let mut late = record("rec-late", RecordKind::Native, &["sess-b"]);
  late.ts = TS_B.into();
  let mut blank = record("rec-blank", RecordKind::Native, &["sess-c"]);
  blank.ts.clear();
  blank.model = "model-blank".into();
  ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", &[empty, early, late, blank]).unwrap();
  let rows = ledger.model_rows().unwrap();
  let by_model = |model: &str| rows.iter().find(|row| row.model == model).unwrap();
  assert_eq!(by_model("model-a").calls, 3);
  assert_eq!((by_model("model-a").first.as_str(), by_model("model-a").last.as_str()), ("2026-01-10", "2026-01-11"));
  assert_eq!((by_model("model-blank").first.as_str(), by_model("model-blank").last.as_str()), ("", ""));
}

#[test]
fn reupserted_native_keeps_first_session_registered() {
  let dir = tempfile::tempdir().unwrap();
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  let first = record("rec-1", RecordKind::Native, &["sess-a"]);
  let second = record("rec-1", RecordKind::Native, &["sess-b"]);
  let mut fallback = record("rec-fb", RecordKind::Fallback, &["sess-a"]);
  fallback.model = "model-fb".into();
  fallback.ts = TS_B.into();
  ledger.record_file(HOST, "/logs/one.jsonl", "sig-1", &[first]).unwrap();
  ledger.record_file(HOST, "/logs/two.jsonl", "sig-2", &[second]).unwrap();
  ledger.record_file(HOST, "/logs/fb.jsonl", "sig-fb", &[fallback]).unwrap();
  let rows = ledger.model_rows().unwrap();
  assert_eq!(models(&rows), ["model-a"]);
}

#[test]
fn rewrite_identical_records_leaves_rows_unchanged() {
  let dir = tempfile::tempdir().unwrap();
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  let mut fallback = record("rec-2", RecordKind::Fallback, &["sess-b"]);
  fallback.ts = TS_B.into();
  let recs = [record("rec-1", RecordKind::Native, &["sess-a"]), fallback];
  ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", &recs).unwrap();
  let before = ledger.model_rows().unwrap();
  ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", &recs).unwrap();
  assert_eq!(ledger.model_rows().unwrap(), before);
  assert_eq!(ledger.captured_sigs(HOST).unwrap(), BTreeMap::from([("/logs/a.jsonl".into(), "sig-a".into())]));
}

#[test]
fn captured_sigs_returns_latest_sig_per_path() {
  let dir = tempfile::tempdir().unwrap();
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  let rec = record("rec-1", RecordKind::Native, &["sess-a"]);
  ledger.record_file("host-a", "/logs/a.jsonl", "sig-old", std::slice::from_ref(&rec)).unwrap();
  ledger.record_file("host-a", "/logs/a.jsonl", "sig-new", std::slice::from_ref(&rec)).unwrap();
  ledger.record_file("host-a", "/logs/b.jsonl", "sig-b", &[rec]).unwrap();
  assert_eq!(
    ledger.captured_sigs("host-a").unwrap(),
    BTreeMap::from([("/logs/a.jsonl".into(), "sig-new".into()), ("/logs/b.jsonl".into(), "sig-b".into())])
  );
}

#[test]
fn fallback_without_sessions_rejected() {
  assert!(record("rec-fb", RecordKind::Fallback, &[]).checked().is_err());
}

#[test]
fn unknown_stored_kind_raises_on_model_rows() {
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
fn native_start_ignores_fallback_only_spans() {
  let dir = tempfile::tempdir().unwrap();
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  // A fallback for the same source seen earlier must not pull the start back: its span rides on a prunable log.
  let fallback = record("rec-fb", RecordKind::Fallback, &["sess-b"]);
  let mut native = record("rec-native", RecordKind::Native, &["sess-a"]);
  native.ts = TS_B.into();
  ledger.record_file(HOST, "/logs/fb.jsonl", "sig-fb", &[fallback]).unwrap();
  ledger.record_file(HOST, "/logs/native.jsonl", "sig-n", &[native]).unwrap();
  let (rows, starts) = ledger.model_rows_with_native_starts().unwrap();
  assert_eq!(starts, BTreeMap::from([(SOURCE.into(), "2026-01-11".into())]));
  assert_eq!(models(&rows), ["model-a"]);
}

#[test]
fn native_start_survives_an_empty_ts_native_row() {
  let dir = tempfile::tempdir().unwrap();
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  let mut undated = record("rec-undated", RecordKind::Native, &["sess-u"]);
  undated.ts.clear();
  let mut dated = record("rec-dated", RecordKind::Native, &["sess-d"]);
  dated.ts = TS_B.into();
  ledger.record_file(HOST, "/logs/u.jsonl", "sig-u", &[undated]).unwrap();
  ledger.record_file(HOST, "/logs/d.jsonl", "sig-d", &[dated]).unwrap();
  let (_rows, starts) = ledger.model_rows_with_native_starts().unwrap();
  assert_eq!(starts, BTreeMap::from([(SOURCE.into(), "2026-01-11".into())]));
}

#[test]
fn agg_read_tracks_the_table_through_every_write_shape() {
  let dir = tempfile::tempdir().unwrap();
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  let moved = record("rec-moved", RecordKind::Native, &["sess-m"]);
  let mut fallback = record("rec-fb", RecordKind::Fallback, &["sess-x"]);
  fallback.model = "model-fb".into();
  fallback.ts = TS_B.into();
  fallback.output = 7;
  let mut native = record("rec-native", RecordKind::Native, &["sess-x"]);
  native.model = "model-native".into();
  let mut empty = record("rec-empty", RecordKind::Native, &["sess-e"]);
  empty.ts.clear();
  empty.model = "model-empty".into();
  ledger.record_file(HOST, "/logs/m.jsonl", "sig-m", &[moved, fallback, empty]).unwrap();
  ledger.model_rows().unwrap(); // the backfill; the aggregate serves from here
  assert_served_on_the_table(&mut ledger);

  let mut moved_grown = record("rec-moved", RecordKind::Native, &["sess-m"]);
  moved_grown.ts = TS_B.into();
  moved_grown.output = 9;
  ledger.record_file(HOST, "/logs/m.jsonl", "sig-m2", &[moved_grown]).unwrap();
  assert_served_on_the_table(&mut ledger);

  let mut same_day = record("rec-same-day", RecordKind::Native, &["sess-sd"]);
  same_day.ts = TS_B.into();
  same_day.output = 6;
  ledger.record_file(HOST, "/logs/sd.jsonl", "sig-sd", &[same_day]).unwrap();
  assert_served_on_the_table(&mut ledger);

  ledger.record_file(HOST, "/logs/n.jsonl", "sig-n", &[native]).unwrap(); // retires rec-fb
  assert_served_on_the_table(&mut ledger);

  let mut born_excluded = record("rec-be", RecordKind::Fallback, &["sess-x"]);
  born_excluded.model = "model-be".into();
  born_excluded.ts = TS_B.into();
  ledger.record_file(HOST, "/logs/be.jsonl", "sig-be", &[born_excluded]).unwrap();
  assert_served_on_the_table(&mut ledger);

  let mut rewritten_excluded = record("rec-fb", RecordKind::Fallback, &["sess-x"]);
  rewritten_excluded.model = "model-fb".into();
  rewritten_excluded.ts = TS_B.into();
  rewritten_excluded.output = 50;
  ledger.record_file(HOST, "/logs/fb.jsonl", "sig-fb2", &[rewritten_excluded]).unwrap();
  assert_served_on_the_table(&mut ledger);

  ledger.conn.execute("DELETE FROM usage WHERE record_id = 'rec-moved'", []).unwrap();
  assert_served_on_the_table(&mut ledger);
}

#[test]
fn previous_release_write_order_never_counts_a_born_excluded_fallback() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("ledger.sqlite3");
  let mut ledger = UsageLedger::open(&path).unwrap();
  let mut native = record("rec-native", RecordKind::Native, &["sess-a"]);
  native.model = "model-native".into();
  ledger.record_file(HOST, "/logs/native.jsonl", "sig-native", &[native]).unwrap();
  let expected = ledger.model_rows().unwrap(); // the backfill runs, the aggregate serves from here
  // The previous release's order, written through raw SQL exactly as its record_file did.
  let raw = Connection::open(&path).unwrap();
  raw
    .execute(
      "INSERT INTO usage (record_id, kind, source, model, account, host, ts, in_fresh, cache_write, cache_read, output, origin, captured_at) VALUES ('rec-fb', 'fallback', ?, 'model-fb', 'acct-a', ?, ?, 10, 2, 3, 7, '/x', ?)",
      params![SOURCE, HOST, TS_B, TS_B],
    )
    .unwrap();
  raw.execute("INSERT INTO fallback_sessions (record_id, session) VALUES ('rec-fb', 'sess-a')", []).unwrap();
  drop(raw);
  let rows = ledger.model_rows().unwrap();
  assert_eq!(models(&expected), ["model-native"]);
  assert_eq!(models(&rows), ["model-native"]);
}

#[test]
fn backfill_prices_a_ledger_written_before_the_aggregate() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("ledger.sqlite3");
  {
    let mut ledger = UsageLedger::open(&path).unwrap();
    ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", &[record("rec-1", RecordKind::Native, &["sess-a"])]).unwrap();
  }
  // Strip the aggregate to the pre-trigger shape, then add a row a raw writer (no triggers) lands.
  {
    let raw = Connection::open(&path).unwrap();
    raw.execute_batch("DROP TABLE usage_agg; DELETE FROM ledger_meta; DROP TRIGGER usage_agg_after_insert;").unwrap();
    raw
      .execute(
        "INSERT INTO usage (record_id, kind, source, model, account, host, ts, in_fresh, cache_write, cache_read, output, origin, captured_at) VALUES ('rec-2', 'native', ?, 'model-b', 'acct-a', ?, ?, 4, 1, 1, 3, '/x', ?)",
        params![SOURCE, HOST, TS_B, TS_B],
      )
      .unwrap();
  }
  let mut ledger = UsageLedger::open(&path).unwrap();
  let rows = ledger.model_rows().unwrap();
  assert_eq!(models(&rows), ["model-a", "model-b"]);
  assert_eq!((rows[0].calls, rows[1].calls), (1, 1));
}

#[test]
fn backfilled_read_serves_the_aggregate_not_the_table_pass() {
  let dir = tempfile::tempdir().unwrap();
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  let rec = record("rec-1", RecordKind::Native, &["sess-a"]);
  ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", &[rec]).unwrap();
  ledger.model_rows().unwrap(); // the first read runs the backfill
  let expected = table_rows(&ledger);
  // Once backfilled the read takes the aggregate, so the table pass never runs.
  assert!(ledger.agg_ready().unwrap());
  assert_eq!(ledger.model_rows().unwrap(), expected);
}

#[test]
fn dropped_aggregate_table_reprices_on_the_next_read() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("ledger.sqlite3");
  let expected = {
    let mut ledger = UsageLedger::open(&path).unwrap();
    ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", &[record("rec-1", RecordKind::Native, &["sess-a"])]).unwrap();
    let rows = ledger.model_rows().unwrap();
    ledger.conn.execute_batch("DROP TABLE usage_agg").unwrap();
    rows
  };
  // The reopen's schema script rebuilds the table, and the read prices the rows that survive.
  let mut ledger = UsageLedger::open(&path).unwrap();
  assert_eq!(ledger.model_rows().unwrap(), expected);
}

#[test]
fn backfilled_aggregate_counts_later_same_group_day_writes() {
  let dir = tempfile::tempdir().unwrap();
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  let first = record("rec-1", RecordKind::Native, &["sess-a"]);
  let mut second = record("rec-2", RecordKind::Native, &["sess-b"]);
  second.output = 6;
  let mut third = record("rec-3", RecordKind::Native, &["sess-c"]);
  third.output = 7;
  ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", &[first]).unwrap();
  ledger.model_rows().unwrap(); // the backfill; the aggregate serves from here
  ledger.record_file(HOST, "/logs/a.jsonl", "sig-a2", &[second, third]).unwrap();
  let served = agg_rows(&mut ledger);
  assert_eq!(served, table_rows(&ledger));
  assert_eq!(served.iter().map(|row| (row.calls, row.output.int())).collect::<Vec<_>>(), [(3, 18)]);
}

#[test]
fn rewrite_within_a_shared_group_day_keeps_the_aggregate_on_the_table() {
  let dir = tempfile::tempdir().unwrap();
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  let records = [
    record("rec-1", RecordKind::Native, &["sess-a"]),
    record("rec-2", RecordKind::Native, &["sess-b"]),
    record("rec-3", RecordKind::Native, &["sess-c"]),
  ];
  ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", &records).unwrap();
  ledger.model_rows().unwrap(); // the backfill
  let mut rewritten = record("rec-2", RecordKind::Native, &["sess-b"]);
  rewritten.output = 50;
  ledger.record_file(HOST, "/logs/a.jsonl", "sig-a2", &[rewritten]).unwrap();
  let served = agg_rows(&mut ledger);
  assert_eq!(served, table_rows(&ledger));
  assert_eq!(served.iter().map(|row| (row.calls, row.output.int())).collect::<Vec<_>>(), [(3, 60)]);
}

#[test]
fn default_ledger_path_derives_from_the_config_home() {
  let home = Path::new("/home/fixture");
  assert_eq!(default_ledger_path(home), home.join("usage").join("ledger.sqlite3"));
}

#[test]
fn ledger_without_index_gains_it_on_reopen() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("ledger.sqlite3");
  let before = {
    let mut ledger = UsageLedger::open(&path).unwrap();
    ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", &[record("rec-1", RecordKind::Native, &["sess-a"])]).unwrap();
    let rows = ledger.model_rows().unwrap();
    ledger.conn.execute_batch("DROP INDEX usage_group_cover").unwrap();
    rows
  };
  let mut ledger = UsageLedger::open(&path).unwrap();
  let indexes = column_text(&ledger.conn, "SELECT name FROM sqlite_master WHERE type = 'index'");
  assert!(indexes.iter().any(|name| name == "usage_group_cover"));
  assert_eq!(ledger.model_rows().unwrap(), before);
}

#[test]
fn model_rows_plan_scans_the_cover_index_without_analyze() {
  let dir = tempfile::tempdir().unwrap();
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  let native = record("rec-native", RecordKind::Native, &["sess-a"]);
  let mut kept = record("rec-fb-kept", RecordKind::Fallback, &["sess-b"]);
  kept.model = "model-kept".into();
  let mut excluded = record("rec-fb-excluded", RecordKind::Fallback, &["sess-a"]);
  excluded.model = "model-excluded".into();
  excluded.ts = TS_B.into();
  ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", &[native, kept, excluded]).unwrap();
  // No ANALYZE has run, so the OR-free NOT EXISTS form must keep the planner off the MULTI-INDEX OR plan.
  assert_eq!(
    column_text(&ledger.conn, "SELECT name FROM sqlite_master WHERE name = 'sqlite_stat1'"),
    Vec::<String>::new()
  );
  let mut statement = ledger.conn.prepare(&format!("EXPLAIN QUERY PLAN {}", sql::model_rows_sql())).unwrap();
  let plan =
    statement.query_map([], |row| row.get::<_, String>(3)).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
  assert!(plan.iter().any(|detail| detail.contains("SCAN") && detail.contains("usage_group_cover")));
  assert!(!plan.iter().any(|detail| detail.contains("MULTI-INDEX OR")));
}

#[test]
fn last_capture_at_is_per_host_and_none_before_the_first_capture() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("ledger.sqlite3");
  let mut ledger = UsageLedger::open(&path).unwrap();
  assert!(ledger.last_capture_at(HOST).unwrap().is_none());
  // The stamp keeps microseconds, so the lower bound is truncated to whole seconds.
  let before = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap();
  ledger.mark_capture_finished(HOST).unwrap();
  let stamped = ledger.last_capture_at(HOST).unwrap().unwrap();
  assert!(before <= stamped && stamped <= OffsetDateTime::now_utc());
  assert!(ledger.last_capture_at("other-host").unwrap().is_none());
  ledger.mark_capture_finished(HOST).unwrap();
  assert!(ledger.last_capture_at(HOST).unwrap().unwrap() >= stamped);
  drop(ledger);
  let reopened = UsageLedger::open(&path).unwrap();
  assert!(reopened.last_capture_at(HOST).unwrap().is_some());
}

#[test]
fn rows_read_reflects_a_write_from_another_instance() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("ledger.sqlite3");
  let first = record("rec-1", RecordKind::Native, &["sess-a"]);
  let mut second = record("rec-2", RecordKind::Native, &["sess-b"]);
  second.model = "model-b".into();
  second.output = 7;
  let rows_before = {
    let mut ledger = UsageLedger::open(&path).unwrap();
    ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", &[first]).unwrap();
    ledger.model_rows_with_native_starts().unwrap().0
  };
  {
    let mut writer = UsageLedger::open(&path).unwrap();
    writer.record_file(HOST, "/logs/b.jsonl", "sig-b", &[second]).unwrap();
  }
  let mut ledger = UsageLedger::open(&path).unwrap();
  let rows_after = ledger.model_rows_with_native_starts().unwrap().0;
  assert_eq!(models(&rows_before), ["model-a"]);
  assert_eq!(models(&rows_after), ["model-b", "model-a"]);
}

#[test]
fn rows_read_reflects_this_instances_own_write() {
  let dir = tempfile::tempdir().unwrap();
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  let first = record("rec-1", RecordKind::Native, &["sess-a"]);
  let mut second = record("rec-2", RecordKind::Native, &["sess-b"]);
  second.model = "model-b".into();
  second.output = 7;
  ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", &[first]).unwrap();
  let rows_before = ledger.model_rows_with_native_starts().unwrap().0;
  ledger.record_file(HOST, "/logs/b.jsonl", "sig-b", &[second]).unwrap();
  let rows_after = ledger.model_rows_with_native_starts().unwrap().0;
  assert_eq!(models(&rows_before), ["model-a"]);
  assert_eq!(models(&rows_after), ["model-b", "model-a"]);
}

#[test]
fn ts_conflict_keeps_the_earlier_non_empty_value() {
  let dir = tempfile::tempdir().unwrap();
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  let first = record("rec-1", RecordKind::Native, &["sess-a"]);
  let mut later = first.clone();
  later.ts = TS_B.into();
  let mut earlier = first.clone();
  earlier.ts = "2026-01-09T07:00:00+00:00".into();
  let mut blank = first.clone();
  blank.ts.clear();
  let stored = |ledger: &UsageLedger| -> String {
    ledger.conn.query_row("SELECT ts FROM usage WHERE record_id = 'rec-1'", [], |row| row.get(0)).unwrap()
  };
  ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", &[first]).unwrap();
  ledger.record_file(HOST, "/logs/a.jsonl", "sig-a2", &[later]).unwrap();
  assert_eq!(stored(&ledger), TS_A);
  ledger.record_file(HOST, "/logs/a.jsonl", "sig-a3", &[earlier]).unwrap();
  assert_eq!(stored(&ledger), "2026-01-09T07:00:00+00:00");
  ledger.record_file(HOST, "/logs/a.jsonl", "sig-a4", &[blank]).unwrap();
  assert_eq!(stored(&ledger), "2026-01-09T07:00:00+00:00");
  assert_eq!(ledger.model_rows().unwrap(), table_rows(&ledger));
}

#[test]
fn in_unsplit_shows_on_every_read_path() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("ledger.sqlite3");
  let mut first = record("rec-1", RecordKind::Native, &["sess-a"]);
  first.in_unsplit = 7;
  let mut second = record("rec-2", RecordKind::Native, &["sess-b"]);
  second.model = "model-b".into();
  second.ts = TS_B.into();
  second.in_unsplit = 7;
  let mut third = record("rec-3", RecordKind::Native, &["sess-c"]);
  third.model = "model-c".into();
  third.in_unsplit = 7;
  let mut ledger = UsageLedger::open(&path).unwrap();
  ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", &[first]).unwrap();
  let table = table_rows(&ledger);
  assert_eq!(table.iter().map(|row| (row.in_unsplit.int(), row.total.int())).collect::<Vec<_>>(), [(7, 27)]);
  ledger.model_rows().unwrap(); // the first read runs the backfill
  ledger.record_file(HOST, "/logs/b.jsonl", "sig-b", &[second]).unwrap();
  let (served, _) = ledger.model_rows_with_native_starts().unwrap();
  let model_b = served.iter().find(|row| row.model == "model-b").unwrap();
  assert_eq!((model_b.in_unsplit.int(), model_b.total.int()), (7, 27));
  {
    let mut foreign = UsageLedger::open(&path).unwrap(); // another process's shape: its own connection
    foreign.record_file(HOST, "/logs/c.jsonl", "sig-c", &[third]).unwrap();
  }
  let served = ledger.model_rows().unwrap();
  assert_eq!(served, table_rows(&ledger));
  let shape = served.iter().map(|row| (row.model.as_str(), row.in_unsplit.int(), row.total.int())).collect::<Vec<_>>();
  assert_eq!(shape, [("model-a", 7, 27), ("model-b", 7, 27), ("model-c", 7, 27)]);
}

#[test]
fn in_unsplit_only_rewrite_keeps_the_aggregate_on_the_table() {
  let dir = tempfile::tempdir().unwrap();
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  let first = record("rec-1", RecordKind::Native, &["sess-a"]);
  let mut rewritten = first.clone();
  rewritten.in_unsplit = 7;
  ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", &[first]).unwrap();
  ledger.model_rows().unwrap(); // the backfill; later reads serve the aggregate
  ledger.record_file(HOST, "/logs/a.jsonl", "sig-a2", &[rewritten]).unwrap();
  let rows = ledger.model_rows().unwrap();
  assert_eq!(rows, table_rows(&ledger));
  assert_eq!(rows.iter().map(|row| (row.in_unsplit.int(), row.total.int())).collect::<Vec<_>>(), [(7, 27)]);
}

#[test]
fn open_of_a_current_ledger_does_not_queue_behind_a_live_writer() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("ledger.sqlite3");
  {
    let mut ledger = UsageLedger::open(&path).unwrap();
    ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", &[record("rec-1", RecordKind::Native, &["sess-a"])]).unwrap();
  }
  // An open write transaction holds the write lock: the open must finish on its own read, not on the writer's wait.
  let raw = Connection::open(&path).unwrap();
  raw.execute_batch("BEGIN IMMEDIATE; CREATE TABLE open_probe (x); INSERT INTO open_probe VALUES (1);").unwrap();
  let started = Instant::now();
  drop(UsageLedger::open(&path).unwrap());
  assert!(started.elapsed() < Duration::from_secs(15));
  raw.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn open_swaps_pre_fix_trigger_bodies_and_reprices_the_aggregate() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("ledger.sqlite3");
  let first = record("rec-1", RecordKind::Native, &["sess-a"]);
  let mut second = record("rec-2", RecordKind::Native, &["sess-b"]);
  second.output = 6;
  pre_fix_ledger(&path, &[first], &[second]); // the backfill ran before second was written
  {
    let raw = Connection::open(&path).unwrap();
    let calls = raw
      .prepare("SELECT calls FROM usage_agg")
      .unwrap()
      .query_map([], |row| row.get::<_, i64>(0))
      .unwrap()
      .collect::<rusqlite::Result<Vec<_>>>()
      .unwrap();
    assert_eq!(calls, [1]); // second's add was dropped
  }
  let mut third = record("rec-3", RecordKind::Native, &["sess-c"]);
  third.output = 7;
  let mut ledger = UsageLedger::open(&path).unwrap();
  let served = agg_rows(&mut ledger);
  assert_eq!(served, table_rows(&ledger));
  assert_eq!(served.iter().map(|row| (row.calls, row.output.int())).collect::<Vec<_>>(), [(2, 11)]);
  ledger.record_file(HOST, "/logs/a.jsonl", "sig-a2", &[third]).unwrap();
  let served = agg_rows(&mut ledger);
  assert_eq!(served, table_rows(&ledger));
  assert_eq!(served.iter().map(|row| (row.calls, row.output.int())).collect::<Vec<_>>(), [(3, 18)]);
  // A reopen whose trigger bodies now match runs nothing: the snapshot and the triggers stay the same.
  ledger.model_rows().unwrap();
  let (before, triggers) = (snapshot(ledger.connection()), trigger_sqls(ledger.connection()));
  drop(ledger);
  let reopened = UsageLedger::open(&path).unwrap();
  assert_eq!(snapshot(reopened.connection()), before);
  assert_eq!(trigger_sqls(reopened.connection()), triggers);
}

#[test]
fn failed_file_keeps_earlier_files_and_drops_its_own() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("ledger.sqlite3");
  let mut ledger = UsageLedger::open(&path).unwrap();
  ledger.conn.execute_batch(REJECT_BAD_USAGE).unwrap();
  let good = record("rec-good", RecordKind::Native, &["sess-good"]);
  ledger.record_file(HOST, "/logs/good.jsonl", "sig-good", &[good]).unwrap();
  let bad = record("rec-bad", RecordKind::Native, &["sess-bad"]);
  assert!(ledger.record_file(HOST, "/logs/bad.jsonl", "sig-bad", &[bad]).is_err());
  assert_eq!(ledger.captured_sigs(HOST).unwrap(), BTreeMap::from([("/logs/good.jsonl".into(), "sig-good".into())]));
  let next = record("rec-next", RecordKind::Native, &["s"]);
  ledger.record_file(HOST, "/logs/next.jsonl", "sig-next", &[next]).unwrap();
  drop(ledger);
  let reopened = UsageLedger::open(&path).unwrap();
  assert_eq!(
    reopened.captured_sigs(HOST).unwrap(),
    BTreeMap::from([("/logs/good.jsonl".into(), "sig-good".into()), ("/logs/next.jsonl".into(), "sig-next".into())])
  );
  assert_eq!(
    column_text(reopened.connection(), "SELECT record_id FROM usage ORDER BY record_id"),
    ["rec-good", "rec-next"]
  );
  let sessions = column_text(reopened.connection(), "SELECT session FROM native_sessions ORDER BY session");
  assert!(!sessions.iter().any(|session| session == "sess-bad"));
}

#[test]
fn batch_lands_every_recorded_file_in_one_commit() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("ledger.sqlite3");
  let mut ledger = UsageLedger::open(&path).unwrap();
  let reader = UsageLedger::open(&path).unwrap();
  let solo = ("/logs/solo.jsonl".to_string(), "sig-solo".to_string());
  ledger.record_file(HOST, &solo.0, &solo.1, &[record("rec-solo", RecordKind::Native, &["s"])]).unwrap();
  assert_eq!(reader.captured_sigs(HOST).unwrap(), BTreeMap::from([solo.clone()]));
  ledger
    .batch(|ledger| {
      ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", &[record("rec-a", RecordKind::Native, &["sess-a"])])?;
      ledger.record_file(HOST, "/logs/b.jsonl", "sig-b", &[record("rec-b", RecordKind::Fallback, &["sess-b"])])?;
      ledger.mark_capture_finished(HOST)?;
      assert_eq!(reader.captured_sigs(HOST)?, BTreeMap::from([solo.clone()]));
      assert!(reader.last_capture_at(HOST)?.is_none());
      Ok(())
    })
    .unwrap();
  assert_eq!(
    reader.captured_sigs(HOST).unwrap(),
    BTreeMap::from([solo.clone(), ("/logs/a.jsonl".into(), "sig-a".into()), ("/logs/b.jsonl".into(), "sig-b".into()),])
  );
  assert!(reader.last_capture_at(HOST).unwrap().is_some());
  drop(ledger);
  drop(reader);
  // The three records share one (source, model) group, so the page rows aggregate them.
  let mut reopened = UsageLedger::open(&path).unwrap();
  assert_eq!(reopened.model_rows().unwrap().iter().map(|row| row.calls).collect::<Vec<_>>(), [3]);
}

#[test]
fn failed_file_inside_a_batch_keeps_earlier_files_and_drops_its_own() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("ledger.sqlite3");
  let mut ledger = UsageLedger::open(&path).unwrap();
  ledger.conn.execute_batch(REJECT_BAD_USAGE).unwrap();
  let result = ledger.batch(|ledger| {
    ledger.record_file(
      HOST,
      "/logs/good.jsonl",
      "sig-good",
      &[record("rec-good", RecordKind::Native, &["sess-good"])],
    )?;
    ledger.record_file(HOST, "/logs/bad.jsonl", "sig-bad", &[record("rec-bad", RecordKind::Native, &["sess-bad"])])?;
    Ok(())
  });
  assert!(result.is_err());
  drop(ledger);
  let reopened = UsageLedger::open(&path).unwrap();
  assert_eq!(reopened.captured_sigs(HOST).unwrap(), BTreeMap::from([("/logs/good.jsonl".into(), "sig-good".into())]));
  assert_eq!(column_text(reopened.connection(), "SELECT record_id FROM usage"), ["rec-good"]);
  let sessions = column_text(reopened.connection(), "SELECT session FROM native_sessions");
  assert!(!sessions.iter().any(|session| session == "sess-bad"));
}

fn stamped(record_id: &str, model: &str, account: &str, ts: &str, output: i64) -> UsageRecord {
  UsageRecord {
    model: model.into(),
    account: account.into(),
    ts: ts.into(),
    output,
    ..record(record_id, RecordKind::Native, &[])
  }
}

/// The counts of each group, sorted by group, so two reads compare group by group.
fn group_counts(rows: Vec<GroupRow>) -> Vec<(GroupKey, i64, [i64; 5])> {
  let mut counts: Vec<_> = rows
    .into_iter()
    .map(|row| {
      let counts = [row.in_fresh, row.cache_write, row.cache_read, row.in_unsplit, row.output].map(Count::int);
      (GroupKey::of(&row), row.calls, counts)
    })
    .collect();
  counts.sort();
  counts
}

/// The last Pacific week before 2026-11-03 is [2026-10-26T07:00Z, 2026-11-02T08:00Z), 169 hours across the fall change.
/// The fixture puts stamps on both edges, a counted fallback, and a fallback that a native record retires after the
/// aggregate is backfilled, so the trigger path is covered as well as the backfill.
#[test]
fn aggregate_week_matches_the_direct_window_query_group_by_group() {
  let dir = tempfile::tempdir().unwrap();
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  let mut retired = record("rec-fb-retired", RecordKind::Fallback, &["sess-x"]);
  retired.model = "model-fb".into();
  retired.account = "acct-fb".into();
  retired.ts = "2026-10-28T10:00:00Z".into();
  let mut kept = record("rec-fb-kept", RecordKind::Fallback, &["sess-y"]);
  kept.model = "model-fb".into();
  kept.account = "acct-fb".into();
  kept.ts = "2026-10-29T12:00:00Z".into();
  let rows = [
    stamped("rec-before", "model-a", "acct-a", "2026-10-26T06:59:59.500Z", 1),
    stamped("rec-edge-in", "model-a", "acct-a", "2026-10-26T07:00:00Z", 2),
    stamped("rec-day", "model-a", "acct-a", "2026-10-27T12:00:00+00:00", 4),
    stamped("rec-fall", "model-a", "acct-a", "2026-11-01T09:00:00Z", 8),
    stamped("rec-last-in", "model-a", "acct-a", "2026-11-02T07:59:59Z", 16),
    stamped("rec-after", "model-a", "acct-a", "2026-11-02T08:00:00Z", 32),
    stamped("rec-other", "model-b", "acct-b", "2026-10-29T10:00:00.000000+00:00", 64),
    retired,
    kept,
  ];
  ledger.record_file(HOST, "/logs/week.jsonl", "sig-week", &rows).unwrap();
  ledger.model_rows().unwrap();
  let native = UsageRecord {
    model: "model-n".into(),
    ts: "2026-10-29T10:00:00Z".into(),
    ..record("rec-native", RecordKind::Native, &["sess-x"])
  };
  ledger.record_file(HOST, "/logs/native.jsonl", "sig-native", &[native]).unwrap();
  assert!(ledger.agg_ready().unwrap(), "the aggregate serves after the backfill");

  let week = Week::last_before("2026-11-03T20:00:00Z".parse().unwrap()).unwrap();
  let window = [Week::utc_text(week.start), Week::utc_text(week.end)];
  let direct = ledger.group_rows(&sql::week_rows_sql(), params![window[0], window[1]]).unwrap();
  let served = ledger.agg_week_rows(week).unwrap();
  assert_eq!(group_counts(served), group_counts(direct));

  let direct = ledger.group_rows(&sql::week_rows_sql(), params![window[0], window[1]]).unwrap();
  let outputs: BTreeMap<String, i64> = rows_from_accs(fold_rows(direct).unwrap())
    .unwrap()
    .0
    .into_iter()
    .map(|row| (row.model, row.output.int()))
    .collect();
  // The edge stamps split at the window, the fall change stays inside it, and the retired fallback is not counted.
  assert_eq!(
    outputs,
    BTreeMap::from([
      ("model-a".to_string(), 2 + 4 + 8 + 16),
      ("model-b".to_string(), 64),
      ("model-fb".to_string(), 5),
      ("model-n".to_string(), 5),
    ])
  );

  // The all-time read takes the same two paths, and both agree on every row, stamps included.
  assert_eq!(
    rows_from_accs(ledger.all_time_accs(true).unwrap()).unwrap(),
    rows_from_accs(ledger.all_time_accs(false).unwrap()).unwrap()
  );
}

/// A row whose stamp names no date still counts, and it bounds no window, so one malformed capture cannot fail the page
/// for as long as the ledger keeps the row. The aggregate path agrees with the table.
#[test]
fn a_stamp_without_a_date_counts_but_bounds_no_window() {
  let dir = tempfile::tempdir().unwrap();
  let mut ledger = UsageLedger::open(dir.path().join("ledger.sqlite3")).unwrap();
  let malformed = stamped("rec-malformed", "model-a", "acct-a", "1", 3);
  let dated = stamped("rec-dated", "model-a", "acct-a", "2026-10-26T06:30:00Z", 4);
  ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", &[malformed, dated]).unwrap();
  let rows = ledger.model_rows().unwrap();
  assert_eq!(rows.len(), 1);
  assert_eq!(rows[0].output.int(), 7);
  // 06:30Z on 10-26 is the Sunday 10-25 in Los Angeles.
  assert_eq!((rows[0].first.as_str(), rows[0].last.as_str()), ("2026-10-25", "2026-10-25"));
  assert_served_on_the_table(&mut ledger);
}
