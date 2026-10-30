pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS usage (
  record_id TEXT PRIMARY KEY,
  kind TEXT NOT NULL,
  source TEXT NOT NULL,
  model TEXT NOT NULL,
  account TEXT NOT NULL,
  host TEXT NOT NULL,
  ts TEXT NOT NULL,
  in_fresh INTEGER NOT NULL,
  cache_write INTEGER NOT NULL,
  cache_read INTEGER NOT NULL,
  in_unsplit INTEGER NOT NULL DEFAULT 0,
  output INTEGER NOT NULL,
  origin TEXT NOT NULL,
  captured_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS usage_group_cover ON usage(kind, source, model, account, ts, in_fresh, cache_write, cache_read, in_unsplit, output);
CREATE TABLE IF NOT EXISTS captured_files (
  host TEXT NOT NULL,
  path TEXT NOT NULL,
  sig TEXT NOT NULL,
  PRIMARY KEY (host, path)
);
CREATE TABLE IF NOT EXISTS native_sessions (
  session TEXT PRIMARY KEY,
  source TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS fallback_sessions (
  record_id TEXT NOT NULL,
  session TEXT NOT NULL,
  PRIMARY KEY (record_id, session)
);
CREATE TABLE IF NOT EXISTS ledger_meta (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS usage_agg (
  source TEXT NOT NULL,
  model TEXT NOT NULL,
  account TEXT NOT NULL,
  kind TEXT NOT NULL,
  day TEXT NOT NULL,
  calls INTEGER NOT NULL,
  in_fresh INTEGER NOT NULL,
  cache_write INTEGER NOT NULL,
  cache_read INTEGER NOT NULL,
  in_unsplit INTEGER NOT NULL DEFAULT 0,
  output INTEGER NOT NULL,
  PRIMARY KEY (source, model, account, kind, day)
);
"#;

const COUNTED_SESSIONS_SQL: &str = "SELECT 1 FROM fallback_sessions fs JOIN native_sessions ns ON ns.session = fs.session WHERE fs.record_id = {r}.record_id";

const AGG_DAY_EXISTS_SQL: &str = "SELECT 1 FROM usage_agg a WHERE a.source = {r}.source AND a.model = {r}.model AND a.account = {r}.account AND a.kind = {r}.kind AND a.day = SUBSTR({r}.ts, 1, 10)";

pub fn agg_contribute_sql(which: &str, calls: &str) -> String {
  let subtract = calls.starts_with('-');
  let sign = if subtract { "-" } else { "" };
  let day_guard =
    if subtract { format!("    AND EXISTS ({})\n", AGG_DAY_EXISTS_SQL.replace("{r}", which)) } else { String::new() };
  format!(
    r#"
  INSERT INTO usage_agg (source, model, account, kind, day, calls, in_fresh, cache_write, cache_read, in_unsplit, output)
  SELECT {which}.source, {which}.model, {which}.account, {which}.kind, SUBSTR({which}.ts, 1, 10), {calls},
         {sign}{which}.in_fresh,
         {sign}{which}.cache_write,
         {sign}{which}.cache_read,
         {sign}{which}.in_unsplit,
         {sign}{which}.output
  WHERE NOT ({which}.kind = 'fallback' AND EXISTS ({counted_sessions}))
{day_guard}  ON CONFLICT(source, model, account, kind, day) DO UPDATE SET
    calls = calls + excluded.calls, in_fresh = in_fresh + excluded.in_fresh,
    cache_write = cache_write + excluded.cache_write, cache_read = cache_read + excluded.cache_read,
    in_unsplit = in_unsplit + excluded.in_unsplit, output = output + excluded.output;"#,
    counted_sessions = COUNTED_SESSIONS_SQL.replace("{r}", which),
  )
}

pub fn agg_trigger_statements() -> Vec<String> {
  vec![
    format!(
      "\nCREATE TRIGGER IF NOT EXISTS usage_agg_after_insert AFTER INSERT ON usage\nBEGIN\n  {}\nEND;",
      agg_contribute_sql("new", "1")
    ),
    format!(
      "\nCREATE TRIGGER IF NOT EXISTS usage_agg_after_update AFTER UPDATE ON usage\nWHEN old.kind <> new.kind OR old.source <> new.source OR old.model <> new.model\n  OR old.account <> new.account OR old.ts <> new.ts OR old.in_fresh <> new.in_fresh\n  OR old.cache_write <> new.cache_write OR old.cache_read <> new.cache_read\n  OR old.in_unsplit <> new.in_unsplit OR old.output <> new.output\nBEGIN\n  {}\n  {}\nEND;",
      agg_contribute_sql("old", "-1"),
      agg_contribute_sql("new", "1")
    ),
    format!(
      "\nCREATE TRIGGER IF NOT EXISTS usage_agg_after_delete AFTER DELETE ON usage\nBEGIN\n  {}\nEND;",
      agg_contribute_sql("old", "-1")
    ),
    r#"
CREATE TRIGGER IF NOT EXISTS usage_agg_after_native_session AFTER INSERT ON native_sessions
BEGIN
  -- A newly registered native session retires every counted fallback row carrying it:
  -- one whose other sessions match no native record, so this insert is the flip. The
  -- subtract fires only when the (group, day) row exists (see _agg_contribute_sql):
  -- before the backfill it is a no-op the backfill subsumes.
  INSERT INTO usage_agg (source, model, account, kind, day, calls, in_fresh, cache_write, cache_read, in_unsplit, output)
  SELECT u.source, u.model, u.account, u.kind, SUBSTR(u.ts, 1, 10), -1,
         -u.in_fresh, -u.cache_write, -u.cache_read, -u.in_unsplit, -u.output
  FROM usage u
  WHERE u.kind = 'fallback'
    AND EXISTS (SELECT 1 FROM usage_agg a WHERE a.source = u.source AND a.model = u.model
                AND a.account = u.account AND a.kind = u.kind AND a.day = SUBSTR(u.ts, 1, 10))
    AND EXISTS (SELECT 1 FROM fallback_sessions fs WHERE fs.record_id = u.record_id AND fs.session = new.session)
    AND NOT EXISTS (
      SELECT 1 FROM fallback_sessions fs2 JOIN native_sessions ns2 ON ns2.session = fs2.session
      WHERE fs2.record_id = u.record_id AND fs2.session <> new.session)
  ON CONFLICT(source, model, account, kind, day) DO UPDATE SET
    calls = calls + excluded.calls, in_fresh = in_fresh + excluded.in_fresh,
    cache_write = cache_write + excluded.cache_write, cache_read = cache_read + excluded.cache_read,
    in_unsplit = in_unsplit + excluded.in_unsplit, output = output + excluded.output;
END;"#
      .to_string(),
    r#"
CREATE TRIGGER IF NOT EXISTS usage_agg_after_fallback_session AFTER INSERT ON fallback_sessions
BEGIN
  -- A fallback's first registered session closes the gap the previous release's write
  -- order leaves: it upserted the usage row before registering sessions, so the insert
  -- trigger's exclusion check saw none and counted a born-excluded record. The row is
  -- present here, so the subtract lands on an existing (group, day) row; this release's
  -- own order (sessions first) reaches this trigger before the usage row exists, where
  -- the SELECT finds nothing and a later registration fails the first-session guard.
  INSERT INTO usage_agg (source, model, account, kind, day, calls, in_fresh, cache_write, cache_read, in_unsplit, output)
  SELECT u.source, u.model, u.account, u.kind, SUBSTR(u.ts, 1, 10), -1,
         -u.in_fresh, -u.cache_write, -u.cache_read, -u.in_unsplit, -u.output
  FROM usage u
  WHERE u.record_id = new.record_id
    AND u.kind = 'fallback'
    AND EXISTS (SELECT 1 FROM usage_agg a WHERE a.source = u.source AND a.model = u.model
                AND a.account = u.account AND a.kind = u.kind AND a.day = SUBSTR(u.ts, 1, 10))
    AND EXISTS (
      SELECT 1 FROM fallback_sessions fs JOIN native_sessions ns ON ns.session = fs.session
      WHERE fs.record_id = u.record_id)
    AND NOT EXISTS (
      SELECT 1 FROM fallback_sessions fs2 WHERE fs2.record_id = u.record_id
      AND fs2.session <> new.session)
  ON CONFLICT(source, model, account, kind, day) DO UPDATE SET
    calls = calls + excluded.calls, in_fresh = in_fresh + excluded.in_fresh,
    cache_write = cache_write + excluded.cache_write, cache_read = cache_read + excluded.cache_read,
    in_unsplit = in_unsplit + excluded.in_unsplit, output = output + excluded.output;
END;"#
      .to_string(),
    r#"
CREATE TRIGGER IF NOT EXISTS usage_agg_purge_empty AFTER UPDATE OF calls ON usage_agg
WHEN new.calls = 0
BEGIN
  DELETE FROM usage_agg WHERE source = new.source AND model = new.model AND account = new.account
    AND kind = new.kind AND day = new.day;
END;"#
      .to_string(),
  ]
}

pub const AGG_SUM_TRIGGER_NAMES: [&str; 5] = [
  "usage_agg_after_insert",
  "usage_agg_after_update",
  "usage_agg_after_delete",
  "usage_agg_after_native_session",
  "usage_agg_after_fallback_session",
];

pub const UPSERT_USAGE_SQL: &str = r#"
INSERT INTO usage (record_id, kind, source, model, account, host, ts,
                   in_fresh, cache_write, cache_read, in_unsplit, output, origin, captured_at)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
ON CONFLICT(record_id) DO UPDATE SET
  kind = excluded.kind,
  source = excluded.source,
  model = excluded.model,
  account = excluded.account,
  host = excluded.host,
  ts = CASE WHEN usage.ts <> '' AND (excluded.ts = '' OR excluded.ts > usage.ts) THEN usage.ts ELSE excluded.ts END,
  in_fresh = excluded.in_fresh,
  cache_write = excluded.cache_write,
  cache_read = excluded.cache_read,
  in_unsplit = excluded.in_unsplit,
  output = excluded.output,
  origin = excluded.origin,
  captured_at = excluded.captured_at
"#;

/// The read side of the exclusion: a usage row counts unless it is a fallback whose session a native record holds. Every
/// read of counted rows (the all-time table, the week reads and the boundary slices) uses this text. The aggregate
/// triggers keep their own form of the same rule, because changing a trigger's text rebuilds it on the live ledger.
const COUNTED_ROW: &str = "NOT EXISTS (SELECT 1 FROM fallback_sessions fs JOIN native_sessions ns ON ns.session = fs.session WHERE u.kind = 'fallback' AND fs.record_id = u.record_id)";

/// A stamp that starts with a date. Only these bound a group's window: a stamp with no date (an empty `ts`, or a malformed
/// one) still counts its row, but gives the row no window date.
const DATED_TS: &str = "'[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]*'";

/// The counted rows grouped per (source, model, account, kind), with the first and last stamp of each group.
pub fn model_rows_sql() -> String {
  format!(
    "SELECT source, model, account, kind, COUNT(*) AS calls, SUM(in_fresh) AS in_fresh, SUM(cache_write) AS cache_write, \
     SUM(cache_read) AS cache_read, SUM(in_unsplit) AS in_unsplit, SUM(output) AS output, \
     MIN(CASE WHEN ts GLOB {DATED_TS} THEN ts END) AS first, MAX(CASE WHEN ts GLOB {DATED_TS} THEN ts END) AS last \
     FROM usage u WHERE {COUNTED_ROW} GROUP BY source, model, account, kind"
  )
}

/// The counted rows of the window `?1 <= ts < ?2` (UTC text), grouped as `model_rows_sql` groups them. A week carries
/// no stamps, so its rows have none.
pub fn week_rows_sql() -> String {
  format!(
    "SELECT source, model, account, kind, COUNT(*) AS calls, SUM(in_fresh) AS in_fresh, SUM(cache_write) AS cache_write, \
     SUM(cache_read) AS cache_read, SUM(in_unsplit) AS in_unsplit, SUM(output) AS output, \
     NULL AS first, NULL AS last \
     FROM usage u WHERE {COUNTED_ROW} AND u.ts >= ?1 AND u.ts < ?2 GROUP BY source, model, account, kind"
  )
}

/// One group's counted rows in the window `?5 <= ts < ?6` (UTC text), for the group `?1` kind, `?2` source, `?3` model,
/// `?4` account. The group's prefix of `usage_group_cover` makes this a range seek, so a boundary slice costs one seek.
pub fn group_slice_sql() -> String {
  format!(
    "SELECT ?2 AS source, ?3 AS model, ?4 AS account, ?1 AS kind, COUNT(*) AS calls, \
     COALESCE(SUM(in_fresh), 0) AS in_fresh, COALESCE(SUM(cache_write), 0) AS cache_write, \
     COALESCE(SUM(cache_read), 0) AS cache_read, COALESCE(SUM(in_unsplit), 0) AS in_unsplit, \
     COALESCE(SUM(output), 0) AS output, NULL AS first, NULL AS last \
     FROM usage u WHERE u.kind = ?1 AND u.source = ?2 AND u.model = ?3 AND u.account = ?4 \
     AND u.ts >= ?5 AND u.ts < ?6 AND {COUNTED_ROW}"
  )
}

/// The first stamp of a group within its first UTC day and the last stamp within its last day (`?5` and `?6`, as
/// `day` values), counted rows only. Each subquery seeks the group's prefix of `usage_group_cover` to one day.
pub fn group_edge_sql() -> String {
  format!(
    "SELECT (SELECT MIN(u.ts) FROM usage u WHERE u.kind = ?1 AND u.source = ?2 AND u.model = ?3 AND u.account = ?4 \
       AND u.ts >= ?5 AND u.ts < date(?5, '+1 day') AND {COUNTED_ROW}), \
     (SELECT MAX(u.ts) FROM usage u WHERE u.kind = ?1 AND u.source = ?2 AND u.model = ?3 AND u.account = ?4 \
       AND u.ts >= ?6 AND u.ts < date(?6, '+1 day') AND {COUNTED_ROW})"
  )
}

/// The aggregate's rows of the days `?1 <= day < ?2`, summed per group. Whole UTC days only: the week read adds the
/// boundary slices of its two end days.
pub const AGG_WEEK_SQL: &str = r#"
SELECT source, model, account, kind,
       SUM(calls) AS calls,
       SUM(in_fresh) AS in_fresh,
       SUM(cache_write) AS cache_write,
       SUM(cache_read) AS cache_read,
       SUM(in_unsplit) AS in_unsplit,
       SUM(output) AS output,
       NULL AS first,
       NULL AS last
FROM usage_agg
WHERE day >= ?1 AND day < ?2
GROUP BY source, model, account, kind
"#;

pub const AGG_ROWS_SQL: &str = r#"
SELECT source, model, account, kind,
       SUM(calls) AS calls,
       SUM(in_fresh) AS in_fresh,
       SUM(cache_write) AS cache_write,
       SUM(cache_read) AS cache_read,
       SUM(in_unsplit) AS in_unsplit,
       SUM(output) AS output,
       MIN(CASE WHEN day GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]' THEN day END) AS first,
       MAX(CASE WHEN day GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]' THEN day END) AS last
FROM usage_agg
GROUP BY source, model, account, kind
"#;

pub const AGG_WIPE_SQL: &str = "DELETE FROM usage_agg";

pub const AGG_BACKFILL_SQL: &str = r#"
INSERT INTO usage_agg (source, model, account, kind, day, calls, in_fresh, cache_write, cache_read, in_unsplit, output)
SELECT source, model, account, kind, SUBSTR(ts, 1, 10) AS day,
       COUNT(*), SUM(in_fresh), SUM(cache_write), SUM(cache_read), SUM(in_unsplit), SUM(output)
FROM usage u

WHERE NOT EXISTS (
  SELECT 1 FROM fallback_sessions fs JOIN native_sessions ns ON ns.session = fs.session
  WHERE u.kind = 'fallback' AND fs.record_id = u.record_id)

GROUP BY source, model, account, kind, day
"#;

pub const CAPTURED_FILE_UPSERT_SQL: &str = "INSERT INTO captured_files (host, path, sig) VALUES (?, ?, ?)\n             ON CONFLICT(host, path) DO UPDATE SET sig = excluded.sig";

pub const LEDGER_META_UPSERT_SQL: &str =
  "INSERT INTO ledger_meta (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value";
