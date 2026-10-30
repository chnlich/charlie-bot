use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, anyhow};
use charliebot_registry::usage::{RecordKind, UsageRecord};
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, params};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::count::Count;
use crate::sql;

const LOCK_WAIT_SECONDS: u64 = 30;

#[derive(Clone, Debug, PartialEq)]
pub struct LedgerAccount {
  pub name: String,
  pub calls: i64,
  pub output: Count,
  pub total: Count,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LedgerRow {
  pub source: String,
  pub model: String,
  pub in_fresh: Count,
  pub cache_write: Count,
  pub cache_read: Count,
  pub in_unsplit: Count,
  pub output: Count,
  pub calls: i64,
  pub total: Count,
  pub first: String,
  pub last: String,
  pub fallback_calls: i64,
  pub fallback_output: Count,
  pub accounts: Vec<LedgerAccount>,
}

/// One usage row as the ledger binds it. The counts and the timestamp keep the SQLite type that the capture binds for
/// them, which `UsageRecord`'s integer and text fields cannot hold.
#[derive(Clone, Debug)]
pub struct StoredRecord {
  pub record_id: String,
  pub kind: RecordKind,
  pub source: String,
  pub model: String,
  pub account: String,
  pub ts: Value,
  pub in_fresh: Count,
  pub cache_write: Count,
  pub cache_read: Count,
  pub output: Count,
  pub in_unsplit: Count,
  pub sessions: Vec<String>,
}

impl From<&UsageRecord> for StoredRecord {
  fn from(record: &UsageRecord) -> Self {
    Self {
      record_id: record.record_id.clone(),
      kind: record.kind,
      source: record.source.clone(),
      model: record.model.clone(),
      account: record.account.clone(),
      ts: Value::Text(record.ts.clone()),
      in_fresh: Count::Int(record.in_fresh),
      cache_write: Count::Int(record.cache_write),
      cache_read: Count::Int(record.cache_read),
      output: Count::Int(record.output),
      in_unsplit: Count::Int(record.in_unsplit),
      sessions: record.sessions.clone(),
    }
  }
}

#[derive(Default)]
struct Sum {
  calls: i64,
  in_fresh: Count,
  cache_write: Count,
  cache_read: Count,
  in_unsplit: Count,
  output: Count,
}

impl Sum {
  fn add(&mut self, row: &GroupRow) -> anyhow::Result<()> {
    self.calls += row.calls;
    self.in_fresh = self.in_fresh.add(row.in_fresh, "usage total")?;
    self.cache_write = self.cache_write.add(row.cache_write, "usage total")?;
    self.cache_read = self.cache_read.add(row.cache_read, "usage total")?;
    self.in_unsplit = self.in_unsplit.add(row.in_unsplit, "usage total")?;
    self.output = self.output.add(row.output, "usage total")?;
    Ok(())
  }

  fn total(&self) -> anyhow::Result<Count> {
    let total = self.in_fresh.add(self.cache_write, "usage total")?;
    let total = total.add(self.cache_read, "usage total")?;
    let total = total.add(self.in_unsplit, "usage total")?;
    total.add(self.output, "usage total")
  }
}

#[derive(Default)]
struct ModelSum {
  sums: Sum,
  fallback_calls: i64,
  fallback_output: Count,
  first: String,
  last: String,
  native_first: String,
  accounts: BTreeMap<String, Sum>,
}

struct GroupRow {
  source: String,
  model: String,
  account: String,
  kind: RecordKind,
  calls: i64,
  in_fresh: Count,
  cache_write: Count,
  cache_read: Count,
  in_unsplit: Count,
  output: Count,
  first: Option<String>,
  last: Option<String>,
}

pub fn default_ledger_path(charliebot_home: &Path) -> PathBuf {
  charliebot_home.join("usage").join("ledger.sqlite3")
}

pub struct UsageLedger {
  path: PathBuf,
  conn: Connection,
}

impl UsageLedger {
  pub fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
    let path = path.as_ref().to_path_buf();
    let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or(Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("creating ledger directory {}", parent.display()))?;
    let conn = Connection::open(&path).with_context(|| format!("opening usage ledger {}", path.display()))?;
    // Python passes `timeouts.USAGE_LEDGER_LOCK_WAIT_SECONDS` to sqlite3.connect at usage_ledger.py:510.
    conn.busy_timeout(Duration::from_secs(LOCK_WAIT_SECONDS))?;
    conn.execute_batch(sql::SCHEMA).with_context(|| format!("creating ledger schema at {}", path.display()))?;
    let mut ledger = Self { path, conn };
    ledger.refresh_agg_triggers()?;
    Ok(ledger)
  }

  pub fn path(&self) -> &Path {
    &self.path
  }

  pub fn close(self) -> anyhow::Result<()> {
    self.conn.close().map_err(|(_, error)| error.into())
  }

  pub fn captured_sigs(&self, host: &str) -> anyhow::Result<BTreeMap<String, String>> {
    let mut statement = self.conn.prepare("SELECT path, sig FROM captured_files WHERE host = ?")?;
    let rows = statement.query_map([host], |row| Ok((row.get(0)?, row.get(1)?)))?;
    rows.collect::<rusqlite::Result<BTreeMap<_, _>>>().map_err(Into::into)
  }

  pub fn record_file(&mut self, host: &str, path: &str, sig: &str, records: &[UsageRecord]) -> anyhow::Result<usize> {
    let stored = records.iter().map(StoredRecord::from).collect::<Vec<_>>();
    self.record_stored_file(host, path, sig, &stored)
  }

  pub(crate) fn record_stored_file(
    &mut self,
    host: &str,
    path: &str,
    sig: &str,
    records: &[StoredRecord],
  ) -> anyhow::Result<usize> {
    let captured_at = utc_isoformat_text(OffsetDateTime::now_utc())?;
    self.conn.execute_batch("SAVEPOINT record_file")?;
    let write_result = (|| -> anyhow::Result<()> {
      for record in records {
        match record.kind {
          RecordKind::Native => {
            for session in &record.sessions {
              self.conn.execute(
                "INSERT OR IGNORE INTO native_sessions (session, source) VALUES (?, ?)",
                params![session, record.source],
              )?;
            }
          }
          RecordKind::Fallback => {
            for session in &record.sessions {
              self.conn.execute(
                "INSERT OR IGNORE INTO fallback_sessions (record_id, session) VALUES (?, ?)",
                params![record.record_id, session],
              )?;
            }
          }
        }
        self.conn.execute(
          sql::UPSERT_USAGE_SQL,
          params![
            record.record_id,
            kind_value(record.kind),
            record.source,
            record.model,
            record.account,
            host,
            record.ts,
            record.in_fresh,
            record.cache_write,
            record.cache_read,
            record.in_unsplit,
            record.output,
            path,
            captured_at,
          ],
        )?;
      }
      self.conn.execute(sql::CAPTURED_FILE_UPSERT_SQL, params![host, path, sig])?;
      Ok(())
    })();
    if let Err(error) = write_result {
      self.conn.execute_batch("ROLLBACK TO record_file")?;
      self.conn.execute_batch("RELEASE record_file")?;
      return Err(error);
    }
    self.conn.execute_batch("RELEASE record_file")?;
    Ok(records.len())
  }

  pub fn batch<T>(&mut self, capture: impl FnOnce(&mut Self) -> anyhow::Result<T>) -> anyhow::Result<T> {
    self.conn.execute_batch("SAVEPOINT usage_capture_batch")?;
    let result = capture(self);
    self.conn.execute_batch("RELEASE usage_capture_batch")?;
    result
  }

  pub fn mark_capture_finished(&mut self, host: &str) -> anyhow::Result<()> {
    self.conn.execute(
      sql::LEDGER_META_UPSERT_SQL,
      params![format!("last_capture_at:{host}"), utc_isoformat_text(OffsetDateTime::now_utc())?],
    )?;
    Ok(())
  }

  pub fn last_capture_at(&self, host: &str) -> anyhow::Result<Option<OffsetDateTime>> {
    let value = self
      .conn
      .query_row("SELECT value FROM ledger_meta WHERE key = ?", [format!("last_capture_at:{host}")], |row| {
        row.get::<_, String>(0)
      })
      .optional()?;
    value
      .map(|value| {
        OffsetDateTime::parse(&value, &time::format_description::well_known::Rfc3339)
          .map_err(|error| anyhow!("invalid ledger timestamp {value:?}: {error}"))
      })
      .transpose()
  }

  pub fn model_rows(&mut self) -> anyhow::Result<Vec<LedgerRow>> {
    self.model_rows_with_native_starts().map(|(rows, _)| rows)
  }

  pub fn model_rows_with_native_starts(&mut self) -> anyhow::Result<(Vec<LedgerRow>, BTreeMap<String, String>)> {
    let accs = match self.agg_accs()? {
      Some(accs) => accs,
      None => self.grouped_accs(sql::MODEL_ROWS_SQL)?,
    };
    rows_from_accs(accs)
  }

  fn refresh_agg_triggers(&mut self) -> anyhow::Result<()> {
    let statements = sql::agg_trigger_statements();
    let expected = sql::AGG_SUM_TRIGGER_NAMES
      .iter()
      .zip(statements.iter())
      .map(|(name, statement)| Ok((*name, stored_trigger_sql(statement)?)))
      .collect::<anyhow::Result<Vec<_>>>()?;
    if triggers_match(&self.conn, &expected)? {
      return Ok(());
    }
    self.conn.execute_batch("BEGIN IMMEDIATE")?;
    if triggers_match(&self.conn, &expected)? {
      self.conn.execute_batch("ROLLBACK")?;
      return Ok(());
    }
    let refresh_result = (|| -> anyhow::Result<()> {
      for name in sql::AGG_SUM_TRIGGER_NAMES {
        self.conn.execute_batch(&format!("DROP TRIGGER IF EXISTS {name}"))?;
      }
      for statement in &statements {
        self.conn.execute_batch(statement)?;
      }
      let backfilled = self
        .conn
        .query_row("SELECT 1 FROM ledger_meta WHERE key = 'agg_backfilled'", [], |_| Ok(()))
        .optional()?
        .is_some();
      if backfilled {
        self.conn.execute_batch(sql::AGG_WIPE_SQL)?;
        self.conn.execute_batch(sql::AGG_BACKFILL_SQL)?;
      }
      Ok(())
    })();
    if let Err(error) = refresh_result {
      self.conn.execute_batch("ROLLBACK")?;
      return Err(error);
    }
    self.conn.execute_batch("COMMIT")?;
    Ok(())
  }

  fn agg_accs(&mut self) -> anyhow::Result<Option<BTreeMap<(String, String), ModelSum>>> {
    let ready = self
      .conn
      .query_row("SELECT value FROM ledger_meta WHERE key = 'agg_backfilled'", [], |_| Ok(()))
      .optional()?
      .is_some();
    let agg_empty = self.conn.query_row("SELECT 1 FROM usage_agg LIMIT 1", [], |_| Ok(())).optional()?.is_none();
    let usage_nonempty = self.conn.query_row("SELECT 1 FROM usage LIMIT 1", [], |_| Ok(())).optional()?.is_some();
    if ready && !(agg_empty && usage_nonempty) {
      return self.grouped_accs(sql::AGG_ROWS_SQL).map(Some);
    }
    self.backfill_agg()?;
    Ok(None)
  }

  fn backfill_agg(&mut self) -> anyhow::Result<()> {
    self.conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| -> anyhow::Result<()> {
      let ready = self
        .conn
        .query_row("SELECT value FROM ledger_meta WHERE key = 'agg_backfilled'", [], |_| Ok(()))
        .optional()?
        .is_some();
      if !ready {
        self.conn.execute_batch(sql::AGG_WIPE_SQL)?;
        self.conn.execute_batch(sql::AGG_BACKFILL_SQL)?;
        self.conn.execute_batch(
          "INSERT INTO ledger_meta (key, value) VALUES ('agg_backfilled', '1') ON CONFLICT(key) DO UPDATE SET value = '1'",
        )?;
      }
      Ok(())
    })();
    if let Err(error) = result {
      self.conn.execute_batch("ROLLBACK")?;
      return Err(error);
    }
    self.conn.execute_batch("COMMIT")?;
    Ok(())
  }

  fn grouped_accs(&self, query: &str) -> anyhow::Result<BTreeMap<(String, String), ModelSum>> {
    let mut statement = self.conn.prepare(query)?;
    let rows = statement.query_map([], read_group_row)?;
    let mut accs = BTreeMap::new();
    for row in rows {
      let row = row?;
      fold_grouped_row(&mut accs, row)?;
    }
    Ok(accs)
  }

  #[cfg(test)]
  pub(crate) fn connection(&self) -> &Connection {
    &self.conn
  }
}

fn kind_value(kind: RecordKind) -> &'static str {
  match kind {
    RecordKind::Native => "native",
    RecordKind::Fallback => "fallback",
  }
}

fn stored_trigger_sql(statement: &str) -> anyhow::Result<String> {
  let body = statement
    .trim()
    .strip_prefix("CREATE TRIGGER IF NOT EXISTS ")
    .ok_or_else(|| anyhow!("aggregate trigger statement has an unexpected prefix"))?;
  Ok(format!("CREATE TRIGGER {body}").trim_end_matches(';').trim_end().to_string())
}

fn triggers_match(conn: &Connection, expected: &[(&str, String)]) -> anyhow::Result<bool> {
  let mut statement = conn.prepare("SELECT name, sql FROM sqlite_master WHERE type = 'trigger'")?;
  let rows = statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
  let stored = rows.collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
  Ok(expected.iter().all(|(name, sql)| stored.get(*name) == Some(sql)))
}

fn read_group_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<GroupRow> {
  let kind_text: String = row.get("kind")?;
  let kind = match kind_text.as_str() {
    "native" => RecordKind::Native,
    "fallback" => RecordKind::Fallback,
    _ => return Err(rusqlite::Error::InvalidColumnType(3, "kind".to_string(), rusqlite::types::Type::Text)),
  };
  Ok(GroupRow {
    source: row.get("source")?,
    model: row.get("model")?,
    account: row.get("account")?,
    kind,
    calls: row.get("calls")?,
    in_fresh: row.get("in_fresh")?,
    cache_write: row.get("cache_write")?,
    cache_read: row.get("cache_read")?,
    in_unsplit: row.get("in_unsplit")?,
    output: row.get("output")?,
    first: row.get("first")?,
    last: row.get("last")?,
  })
}

fn fold_grouped_row(accs: &mut BTreeMap<(String, String), ModelSum>, row: GroupRow) -> anyhow::Result<()> {
  let key = (row.source.clone(), row.model.clone());
  let acc = accs.entry(key).or_default();
  acc.sums.add(&row)?;
  if let Some(first) = row.first.as_deref().filter(|first| !first.is_empty())
    && (acc.first.is_empty() || first < acc.first.as_str())
  {
    acc.first = first.to_string();
  }
  if let Some(last) = row.last.as_deref().filter(|last| !last.is_empty())
    && (acc.last.is_empty() || last > acc.last.as_str())
  {
    acc.last = last.to_string();
  }
  if row.kind == RecordKind::Fallback {
    acc.fallback_calls += row.calls;
    acc.fallback_output = acc.fallback_output.add(row.output, "fallback output total")?;
  }
  if row.kind == RecordKind::Native
    && let Some(first) = row.first.as_deref().filter(|first| !first.is_empty())
    && (acc.native_first.is_empty() || first < acc.native_first.as_str())
  {
    acc.native_first = first.to_string();
  }
  acc.accounts.entry(row.account.clone()).or_default().add(&row)?;
  Ok(())
}

fn rows_from_accs(
  accs: BTreeMap<(String, String), ModelSum>,
) -> anyhow::Result<(Vec<LedgerRow>, BTreeMap<String, String>)> {
  let mut starts = BTreeMap::new();
  let mut rows = Vec::with_capacity(accs.len());
  for ((source, model), acc) in accs {
    if !acc.native_first.is_empty() {
      starts
        .entry(source.clone())
        .and_modify(|start: &mut String| {
          if acc.native_first.as_str() < start.as_str() {
            *start = acc.native_first.clone();
          }
        })
        .or_insert_with(|| acc.native_first.clone());
    }
    let mut accounts = Vec::with_capacity(acc.accounts.len());
    for (name, sums) in acc.accounts {
      accounts.push(LedgerAccount { name, calls: sums.calls, output: sums.output, total: sums.total()? });
    }
    accounts.sort_by(|left, right| right.total.compare(left.total).then_with(|| left.name.cmp(&right.name)));
    rows.push(LedgerRow {
      source,
      model,
      in_fresh: acc.sums.in_fresh,
      cache_write: acc.sums.cache_write,
      cache_read: acc.sums.cache_read,
      in_unsplit: acc.sums.in_unsplit,
      output: acc.sums.output,
      calls: acc.sums.calls,
      total: acc.sums.total()?,
      first: acc.first,
      last: acc.last,
      fallback_calls: acc.fallback_calls,
      fallback_output: acc.fallback_output,
      accounts,
    });
  }
  rows.sort_by(|left, right| {
    right
      .total
      .compare(left.total)
      .then_with(|| left.source.cmp(&right.source))
      .then_with(|| left.model.cmp(&right.model))
  });
  Ok((rows, starts))
}

fn utc_isoformat_text(now: OffsetDateTime) -> anyhow::Result<String> {
  let micros = now.nanosecond() / 1_000;
  let now = now.replace_nanosecond(micros * 1_000)?;
  let formatted = now.format(&Rfc3339)?;
  let without_zone = formatted.strip_suffix('Z').expect("UTC RFC3339 ends with Z");
  Ok(if micros == 0 {
    format!("{without_zone}+00:00")
  } else {
    let (seconds, fraction) =
      without_zone.rsplit_once('.').expect("RFC3339 has a fractional part when microseconds are nonzero");
    format!("{seconds}.{fraction:0<6}+00:00")
  })
}

#[cfg(test)]
#[path = "ledger_tests.rs"]
mod tests;
