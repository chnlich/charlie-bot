-- The sqlite_master.sql text of every object in <home>/usage/ledger.sqlite3, dumped from a ledger that
-- src/features/usage/usage_ledger.py created at baseline 4521a51b2. Objects are separated by one blank
-- line. Each object's stored text runs from its CREATE line to the end of its block, minus the final ';'
-- this dump appends (inner ';' inside trigger bodies are part of the text). The five usage_agg_after_*
-- trigger texts are compared with sqlite_master.sql byte for byte on every open, including the lines inside
-- BEGIN that hold exactly two spaces; see files.md, section 5.8.

CREATE TABLE usage (
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

CREATE TABLE captured_files (
  host TEXT NOT NULL,
  path TEXT NOT NULL,
  sig TEXT NOT NULL,
  PRIMARY KEY (host, path)
);

CREATE TABLE native_sessions (
  session TEXT PRIMARY KEY,
  source TEXT NOT NULL
);

CREATE TABLE fallback_sessions (
  record_id TEXT NOT NULL,
  session TEXT NOT NULL,
  PRIMARY KEY (record_id, session)
);

CREATE TABLE ledger_meta (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE usage_agg (
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

CREATE INDEX usage_group_cover ON usage(kind, source, model, account, ts, in_fresh, cache_write, cache_read, in_unsplit, output);

CREATE TRIGGER usage_agg_after_insert AFTER INSERT ON usage
BEGIN
  
  INSERT INTO usage_agg (source, model, account, kind, day, calls, in_fresh, cache_write, cache_read, in_unsplit, output)
  SELECT new.source, new.model, new.account, new.kind, SUBSTR(new.ts, 1, 10), 1,
         new.in_fresh,
         new.cache_write,
         new.cache_read,
         new.in_unsplit,
         new.output
  WHERE NOT (new.kind = 'fallback' AND EXISTS (SELECT 1 FROM fallback_sessions fs JOIN native_sessions ns ON ns.session = fs.session WHERE fs.record_id = new.record_id))
  ON CONFLICT(source, model, account, kind, day) DO UPDATE SET
    calls = calls + excluded.calls, in_fresh = in_fresh + excluded.in_fresh,
    cache_write = cache_write + excluded.cache_write, cache_read = cache_read + excluded.cache_read,
    in_unsplit = in_unsplit + excluded.in_unsplit, output = output + excluded.output;
END;

CREATE TRIGGER usage_agg_after_update AFTER UPDATE ON usage
WHEN old.kind <> new.kind OR old.source <> new.source OR old.model <> new.model
  OR old.account <> new.account OR old.ts <> new.ts OR old.in_fresh <> new.in_fresh
  OR old.cache_write <> new.cache_write OR old.cache_read <> new.cache_read
  OR old.in_unsplit <> new.in_unsplit OR old.output <> new.output
BEGIN
  
  INSERT INTO usage_agg (source, model, account, kind, day, calls, in_fresh, cache_write, cache_read, in_unsplit, output)
  SELECT old.source, old.model, old.account, old.kind, SUBSTR(old.ts, 1, 10), -1,
         -old.in_fresh,
         -old.cache_write,
         -old.cache_read,
         -old.in_unsplit,
         -old.output
  WHERE NOT (old.kind = 'fallback' AND EXISTS (SELECT 1 FROM fallback_sessions fs JOIN native_sessions ns ON ns.session = fs.session WHERE fs.record_id = old.record_id))
    AND EXISTS (SELECT 1 FROM usage_agg a WHERE a.source = old.source AND a.model = old.model AND a.account = old.account AND a.kind = old.kind AND a.day = SUBSTR(old.ts, 1, 10))
  ON CONFLICT(source, model, account, kind, day) DO UPDATE SET
    calls = calls + excluded.calls, in_fresh = in_fresh + excluded.in_fresh,
    cache_write = cache_write + excluded.cache_write, cache_read = cache_read + excluded.cache_read,
    in_unsplit = in_unsplit + excluded.in_unsplit, output = output + excluded.output;
  
  INSERT INTO usage_agg (source, model, account, kind, day, calls, in_fresh, cache_write, cache_read, in_unsplit, output)
  SELECT new.source, new.model, new.account, new.kind, SUBSTR(new.ts, 1, 10), 1,
         new.in_fresh,
         new.cache_write,
         new.cache_read,
         new.in_unsplit,
         new.output
  WHERE NOT (new.kind = 'fallback' AND EXISTS (SELECT 1 FROM fallback_sessions fs JOIN native_sessions ns ON ns.session = fs.session WHERE fs.record_id = new.record_id))
  ON CONFLICT(source, model, account, kind, day) DO UPDATE SET
    calls = calls + excluded.calls, in_fresh = in_fresh + excluded.in_fresh,
    cache_write = cache_write + excluded.cache_write, cache_read = cache_read + excluded.cache_read,
    in_unsplit = in_unsplit + excluded.in_unsplit, output = output + excluded.output;
END;

CREATE TRIGGER usage_agg_after_delete AFTER DELETE ON usage
BEGIN
  
  INSERT INTO usage_agg (source, model, account, kind, day, calls, in_fresh, cache_write, cache_read, in_unsplit, output)
  SELECT old.source, old.model, old.account, old.kind, SUBSTR(old.ts, 1, 10), -1,
         -old.in_fresh,
         -old.cache_write,
         -old.cache_read,
         -old.in_unsplit,
         -old.output
  WHERE NOT (old.kind = 'fallback' AND EXISTS (SELECT 1 FROM fallback_sessions fs JOIN native_sessions ns ON ns.session = fs.session WHERE fs.record_id = old.record_id))
    AND EXISTS (SELECT 1 FROM usage_agg a WHERE a.source = old.source AND a.model = old.model AND a.account = old.account AND a.kind = old.kind AND a.day = SUBSTR(old.ts, 1, 10))
  ON CONFLICT(source, model, account, kind, day) DO UPDATE SET
    calls = calls + excluded.calls, in_fresh = in_fresh + excluded.in_fresh,
    cache_write = cache_write + excluded.cache_write, cache_read = cache_read + excluded.cache_read,
    in_unsplit = in_unsplit + excluded.in_unsplit, output = output + excluded.output;
END;

CREATE TRIGGER usage_agg_after_native_session AFTER INSERT ON native_sessions
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
END;

CREATE TRIGGER usage_agg_after_fallback_session AFTER INSERT ON fallback_sessions
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
END;

CREATE TRIGGER usage_agg_purge_empty AFTER UPDATE OF calls ON usage_agg
WHEN new.calls = 0
BEGIN
  DELETE FROM usage_agg WHERE source = new.source AND model = new.model AND account = new.account
    AND kind = new.kind AND day = new.day;
END;

