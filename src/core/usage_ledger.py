"""SQLite usage ledger: one row per API call, aggregated for the /token-usage page.

The /token-usage page numbers must survive deletion of the source logs they were
parsed from, so every captured call is copied into this ledger once and the page
rows are recomputed from the ledger alone. A record is deduped on ``record_id``
(Claude-style message id or equivalent), so the same call replayed by several
captured files on several hosts is stored and counted once.

A record is NATIVE when the CLI's own log — a file the CLI keeps and serves its
history from — carries it; FALLBACK when the usage is known only from a
charlie-bot capture whose underlying CLI log may already be pruned. A fallback
record is counted only while none of the session ids it carries has a NATIVE
record of its own: the moment the CLI's log for any of those sessions is seen,
the fallback's contribution is presumed restated there. The exclusion is a
query-time rule over the registered sessions, not a deletion — re-reading a
pruned-away file's captured rows and their spans stays possible forever. A usage
row leaves the ledger only when a re-parse of its own source supersedes it by id
prefix (``record_file``'s ``supersede_prefix``); nothing else deletes usage rows.
(The only other DELETE in the schema reaps the aggregate's own zero-count day
rows.)

``in_unsplit`` holds input tokens whose cache hit/miss split was never logged, and it
counts toward every total like the other input columns. The one-time schema-2 upgrade
(run on open while ``ledger_meta`` has no ``schema`` key) moves a ``master:`` or native
``thread:`` record's input into it when the record's source file is gone -- the one
case where the split can never be recovered. A record whose file remains keeps its
input in ``in_fresh``: the file can still be re-parsed with the correct split.

``captured_files`` remembers the last signature written per (host, path), so a
collector can skip files it already ingested by content, not by existence.

``capture_gates`` remembers one source's *probe* per (host, path): the file-state
pairs a probe ran under and the signature it computed, so a fresh process reuses
that probe while the files sit byte-still instead of re-scanning the source.

``usage_agg`` carries the page's aggregates at (group, day) granularity, maintained
by the schema's triggers for every writer: the triggers ride the database, so any
process's capture -- this module at any version, the CLI, the server -- keeps the
aggregate exact without a per-read freshness witness. The page read serves it
whenever the one-time backfill has run, and prices the table directly before that.
"""

from __future__ import annotations

import sqlite3
from collections.abc import Sequence
from dataclasses import dataclass, field
from datetime import UTC, datetime
from enum import StrEnum
from pathlib import Path
from typing import Self

from src.core.timeouts import USAGE_LEDGER_LOCK_WAIT_SECONDS

_SCHEMA = """
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
CREATE TABLE IF NOT EXISTS capture_gates (
  host TEXT NOT NULL,
  path TEXT NOT NULL,
  main_size INTEGER NOT NULL,
  main_mtime_ns INTEGER NOT NULL,
  wal_size INTEGER,
  wal_mtime_ns INTEGER,
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
"""

# The aggregate's maintenance contract: every usage write rides the ledger's own
# statements, and these triggers ride the database, so the aggregate stays exact for
# every writer -- this module at any version, in any process -- with no per-read
# freshness witness. A counted record contributes one (group, day) row per day its ts
# carries; the day granularity is what makes a rewrite correctable, because subtracting
# the old (group, day) row and adding the new one keeps the group's MIN/MAX day exact,
# which group-level sums alone cannot do. A fallback record counts only while none of
# its registered sessions has a native record -- the same rule the page read applies,
# evaluated at write time from the session tables; a fallback registers its sessions
# before its usage row lands so that check sees them. A day row whose calls reach zero
# is deleted (the only DELETE in the schema's triggers), because a zero row would otherwise extend
# its group's MIN/MAX day and resurrect a group the table no longer counts.
_COUNTED_SESSIONS_SQL = (
    "SELECT 1 FROM fallback_sessions fs JOIN native_sessions ns ON ns.session = fs.session"
    " WHERE fs.record_id = {r}.record_id")

_AGG_DAY_EXISTS_SQL = (
    "SELECT 1 FROM usage_agg a WHERE a.source = {r}.source AND a.model = {r}.model"
    " AND a.account = {r}.account AND a.kind = {r}.kind AND a.day = SUBSTR({r}.ts, 1, 10)")


def _agg_contribute_sql(which: str, calls: str) -> str:
  """One aggregate upsert: add (or, with a negative calls term, subtract) {which} row's
  contribution, skipping a fallback row the session tables exclude. An add lands
  unconditionally on its (group, day) row -- creating it or upserting onto the one already
  there -- so every counted write increments the aggregate. A subtract fires only when the
  (group, day) row already exists: before the backfill the aggregate is empty and every
  subtract is a no-op the backfill's wholesale replacement subsumes, while a subtract that
  could create a row would plant a negative orphan no later pass removes."""
  subtract = calls.startswith("-")
  sign = "-" if subtract else ""
  day_guard = f"    AND EXISTS ({_AGG_DAY_EXISTS_SQL.format(r=which)})\n" if subtract else ""
  return f"""
  INSERT INTO usage_agg (source, model, account, kind, day, calls, in_fresh, cache_write, cache_read, in_unsplit, output)
  SELECT {which}.source, {which}.model, {which}.account, {which}.kind, SUBSTR({which}.ts, 1, 10), {calls},
         {sign}{which}.in_fresh,
         {sign}{which}.cache_write,
         {sign}{which}.cache_read,
         {sign}{which}.in_unsplit,
         {sign}{which}.output
  WHERE NOT ({which}.kind = 'fallback' AND EXISTS ({_COUNTED_SESSIONS_SQL.format(r=which)}))
{day_guard}  ON CONFLICT(source, model, account, kind, day) DO UPDATE SET
    calls = calls + excluded.calls, in_fresh = in_fresh + excluded.in_fresh,
    cache_write = cache_write + excluded.cache_write, cache_read = cache_read + excluded.cache_read,
    in_unsplit = in_unsplit + excluded.in_unsplit, output = output + excluded.output;"""


_AGG_TRIGGER_STATEMENTS = (
    f"""
CREATE TRIGGER IF NOT EXISTS usage_agg_after_insert AFTER INSERT ON usage
BEGIN
  {_agg_contribute_sql('new', '1')}
END;""",
    f"""
CREATE TRIGGER IF NOT EXISTS usage_agg_after_update AFTER UPDATE ON usage
WHEN old.kind <> new.kind OR old.source <> new.source OR old.model <> new.model
  OR old.account <> new.account OR old.ts <> new.ts OR old.in_fresh <> new.in_fresh
  OR old.cache_write <> new.cache_write OR old.cache_read <> new.cache_read
  OR old.in_unsplit <> new.in_unsplit OR old.output <> new.output
BEGIN
  {_agg_contribute_sql('old', '-1')}
  {_agg_contribute_sql('new', '1')}
END;""",
    f"""
CREATE TRIGGER IF NOT EXISTS usage_agg_after_delete AFTER DELETE ON usage
BEGIN
  {_agg_contribute_sql('old', '-1')}
END;""",
    """
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
END;""",
    """
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
END;""",
    """
CREATE TRIGGER IF NOT EXISTS usage_agg_purge_empty AFTER UPDATE OF calls ON usage_agg
WHEN new.calls = 0
BEGIN
  DELETE FROM usage_agg WHERE source = new.source AND model = new.model AND account = new.account
    AND kind = new.kind AND day = new.day;
END;""",
)

# The five triggers that sum the token columns. The schema-2 upgrade drops these by name
# before the move: CREATE TRIGGER IF NOT EXISTS never replaces an existing trigger, so a
# ledger upgraded in place would otherwise keep four-sum trigger bodies forever. The
# open-time refresh (``_refresh_agg_triggers``) compares stored bodies against this
# module's for the same reason.
_AGG_SUM_TRIGGER_NAMES = (
    "usage_agg_after_insert",
    "usage_agg_after_update",
    "usage_agg_after_delete",
    "usage_agg_after_native_session",
    "usage_agg_after_fallback_session",
)


def _stored_trigger_sql(statement: str) -> str:
  """The sqlite_master.sql text creating a trigger from *statement* leaves behind: SQLite
  keeps the statement's text minus its IF NOT EXISTS clause, edge whitespace, and the
  trailing semicolon."""
  body = statement.strip()[len("CREATE TRIGGER IF NOT EXISTS "):]
  return ("CREATE TRIGGER " + body).rstrip(";").rstrip()


# Each token-summing trigger name paired with the sqlite_master.sql text this module's own
# statement leaves behind -- the comparison the open-time refresh runs.
# The name list deliberately excludes usage_agg_purge_empty, so the lengths differ by design.
_AGG_TRIGGER_SQLS = {
    name: _stored_trigger_sql(statement) for name, statement in zip(_AGG_SUM_TRIGGER_NAMES, _AGG_TRIGGER_STATEMENTS, strict=False)
}

# The usage row is upserted whole on a repeated record_id (the same API call seen again
# from another file or host): the latest capture wins on every non-key column but ts,
# which record_file keeps at the earlier non-empty value.
_UPSERT_USAGE_SQL = """
INSERT INTO usage (record_id, kind, source, model, account, host, ts,
                   in_fresh, cache_write, cache_read, in_unsplit, output, origin, captured_at)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
ON CONFLICT(record_id) DO UPDATE SET
  kind = excluded.kind,
  source = excluded.source,
  model = excluded.model,
  account = excluded.account,
  host = excluded.host,
  ts = excluded.ts,
  in_fresh = excluded.in_fresh,
  cache_write = excluded.cache_write,
  cache_read = excluded.cache_read,
  in_unsplit = excluded.in_unsplit,
  output = excluded.output,
  origin = excluded.origin,
  captured_at = excluded.captured_at
"""

# Counted records: every NATIVE row, plus each FALLBACK row none of whose registered
# sessions has a NATIVE record (the any-match EXISTS join). The OR-free NOT EXISTS
# form is equivalent — for a NATIVE row the subquery's u.kind = 'fallback' arm is
# false, so NOT EXISTS keeps it — and lets the planner scan the covering index
# usage_group_cover for the GROUP BY outright instead of a MULTI-INDEX OR with a temp
# B-tree, a plan that also leans on ANALYZE statistics this ledger does not keep. An
# unknown stored kind is rejected at read time by the RecordKind conversion over the
# grouped rows this query returns.
_COUNTED_WHERE_SQL = """
WHERE NOT EXISTS (
  SELECT 1 FROM fallback_sessions fs JOIN native_sessions ns ON ns.session = fs.session
  WHERE u.kind = 'fallback' AND fs.record_id = u.record_id)
"""

# The /token-usage page aggregate: one grouped query over the counted records, so the
# page load folds a few (source, model, account, kind) groups in Python instead of
# streaming every counted record through it. ``first``/``last`` come from the dated
# ts prefixes only: NULLIF keeps an empty ts from anchoring either end.
_MODEL_ROWS_SQL = f"""
SELECT source, model, account, kind,
       COUNT(*) AS calls,
       SUM(in_fresh) AS in_fresh,
       SUM(cache_write) AS cache_write,
       SUM(cache_read) AS cache_read,
       SUM(in_unsplit) AS in_unsplit,
       SUM(output) AS output,
       MIN(NULLIF(SUBSTR(ts, 1, 10), '')) AS first,
       MAX(NULLIF(SUBSTR(ts, 1, 10), '')) AS last
FROM usage u
{_COUNTED_WHERE_SQL}
GROUP BY source, model, account, kind
"""

# The currently-excluded fallback rows. The fold diffs this set against the memo's
# and stands down to the full pass on any difference: retirement cannot be patched
# incrementally, so any set change re-prices the page from the table.
_EXCLUDED_FALLBACK_IDS_SQL = """
SELECT u.record_id FROM usage u WHERE u.kind = 'fallback' AND EXISTS (
  SELECT 1 FROM fallback_sessions fs JOIN native_sessions ns ON ns.session = fs.session
  WHERE fs.record_id = u.record_id)
"""

# The page read over the maintained aggregate: same grouped shape as the table pass
# above, over one row per (group, day) instead of one per record. Zero-count day rows
# are deleted by the schema's purge trigger, so no filter here.
_AGG_ROWS_SQL = """
SELECT source, model, account, kind,
       SUM(calls) AS calls,
       SUM(in_fresh) AS in_fresh,
       SUM(cache_write) AS cache_write,
       SUM(cache_read) AS cache_read,
       SUM(in_unsplit) AS in_unsplit,
       SUM(output) AS output,
       MIN(NULLIF(day, '')) AS first,
       MAX(NULLIF(day, '')) AS last
FROM usage_agg
GROUP BY source, model, account, kind
"""

# The wholesale aggregate replacement, run inside the caller's locked transaction by the
# one-time backfill, the schema-2 upgrade's re-price, and the trigger refresh: the caller
# deletes every usage_agg row first (``_AGG_WIPE_SQL``), then this counted-row pass -- the
# table pass's counted-row rule projected to (group, day) granularity -- rebuilds the
# aggregate from the table. Increments the triggers wrote before the pass never survive
# beside it, and neither does any row an older trigger body left behind.
_AGG_WIPE_SQL = "DELETE FROM usage_agg"
_AGG_BACKFILL_SQL = f"""
INSERT INTO usage_agg (source, model, account, kind, day, calls, in_fresh, cache_write, cache_read, in_unsplit, output)
SELECT source, model, account, kind, SUBSTR(ts, 1, 10) AS day,
       COUNT(*), SUM(in_fresh), SUM(cache_write), SUM(cache_read), SUM(in_unsplit), SUM(output)
FROM usage u
{_COUNTED_WHERE_SQL}
GROUP BY source, model, account, kind, day
"""

# The records whose input the schema-2 upgrade may move into in_unsplit once their source
# file is gone: the manager's master: records and the native thread: ones. codex: records
# carry their rollout's own hit/miss split, so they never move.
_UPGRADE_MOVE_RECORDS_SQL = "(record_id GLOB 'master:*' OR (record_id GLOB 'thread:*' AND kind = 'native'))"


class RecordKind(StrEnum):
  """How a usage row was learned: from a CLI-kept log (native) or from a charlie-bot
  capture whose underlying CLI log may be pruned (fallback)."""

  NATIVE = "native"
  FALLBACK = "fallback"


@dataclass(frozen=True, slots=True)
class UsageRecord:
  """One API call's usage as extracted from a captured file.

  ``ts`` is ISO 8601 UTC; ``sessions`` names the CLI sessions the call's log belongs
  to, and a fallback record must carry at least one — exclusion is keyed on them.
  ``in_unsplit`` is input whose cache hit/miss split was never logged; it counts toward
  every total like the split input columns.
  """

  record_id: str
  kind: RecordKind
  source: str
  model: str
  account: str
  ts: str
  in_fresh: int
  cache_write: int
  cache_read: int
  output: int
  in_unsplit: int = 0
  sessions: tuple[str, ...] = ()

  def __post_init__(self) -> None:
    if self.kind == RecordKind.FALLBACK and not self.sessions:
      raise ValueError(f"fallback record {self.record_id!r} carries no sessions")


@dataclass(frozen=True)
class LedgerAccount:
  """Usage for one subscription account of a model."""

  name: str
  calls: int
  output: int
  total: int


@dataclass(frozen=True)
class LedgerRow:
  """One aggregated per-model row for the /token-usage page.

  ``in_unsplit`` is input whose cache hit/miss split was never logged; it counts
  toward ``total`` like the other input columns. ``fallback_calls``/``fallback_output``
  cover only the counted FALLBACK rows behind this row — the part of the row that could
  still change if a pruned CLI log resurfaces and retires the fallbacks.
  """

  source: str
  model: str
  in_fresh: int
  cache_write: int
  cache_read: int
  in_unsplit: int
  output: int
  calls: int
  total: int
  first: str
  last: str
  fallback_calls: int
  fallback_output: int
  accounts: list[LedgerAccount]


@dataclass
class _Sum:
  """Mutable token accumulator for one bucket; ``total`` is the five token fields.

  ``add`` folds one aggregate group row, whose ``calls`` column carries the group's
  record count.
  """

  calls: int = 0
  in_fresh: int = 0
  cache_write: int = 0
  cache_read: int = 0
  in_unsplit: int = 0
  output: int = 0

  def add(self, row: sqlite3.Row) -> None:
    self.calls += row["calls"]
    self.in_fresh += row["in_fresh"]
    self.cache_write += row["cache_write"]
    self.cache_read += row["cache_read"]
    self.in_unsplit += row["in_unsplit"]
    self.output += row["output"]

  @property
  def total(self) -> int:
    return self.in_fresh + self.cache_write + self.cache_read + self.in_unsplit + self.output


@dataclass
class _ModelSum:
  """Per-(source, model) accumulator: every counted row plus the fallback split.

  ``native_first`` is the earliest dated day across the group's NATIVE rows alone,
  so the source's native start can be re-derived after a fold without re-reading
  the table.
  """

  sums: _Sum = field(default_factory=_Sum)
  fallback_calls: int = 0
  fallback_output: int = 0
  first: str = ""
  last: str = ""
  native_first: str = ""
  accounts: dict[str, _Sum] = field(default_factory=dict)


def _fold_grouped_row(accs: dict[tuple[str, str], _ModelSum], row: sqlite3.Row | dict) -> None:
  """Fold one grouped pass row (calls carries the group's record count) into the accs."""
  kind = RecordKind(row["kind"])
  acc = accs.setdefault((row["source"], row["model"]), _ModelSum())
  acc.sums.add(row)
  first, last = row["first"], row["last"]  # NULL when the group has no dated ts
  if first and (not acc.first or first < acc.first):
    acc.first = first
  if last and (not acc.last or last > acc.last):
    acc.last = last
  if kind is RecordKind.FALLBACK:
    acc.fallback_calls += row["calls"]
    acc.fallback_output += row["output"]
  if kind is RecordKind.NATIVE and first and (not acc.native_first or first < acc.native_first):
    acc.native_first = first
  acc.accounts.setdefault(row["account"], _Sum()).add(row)


def _fold_raw_row(accs: dict[tuple[str, str], _ModelSum], row: sqlite3.Row) -> None:
  """Fold one raw usage row (the delta read's shape, one record) into the accs."""
  day = row["ts"][:10] or None  # the grouped pass's NULLIF(SUBSTR(ts, 1, 10), '')
  grouped = {
      k: row[k]
      for k in ("source", "model", "account", "kind", "in_fresh", "cache_write", "cache_read", "in_unsplit", "output")
  }
  grouped.update(calls=1, first=day, last=day)
  _fold_grouped_row(accs, grouped)


def _rows_from_accs(accs: dict[tuple[str, str], _ModelSum]) -> list[LedgerRow]:
  """The page rows from the accumulators, sorted by total descending with the keys as tiebreakers."""
  rows = [
      LedgerRow(
          source=source,
          model=model,
          in_fresh=acc.sums.in_fresh,
          cache_write=acc.sums.cache_write,
          cache_read=acc.sums.cache_read,
          in_unsplit=acc.sums.in_unsplit,
          output=acc.sums.output,
          calls=acc.sums.calls,
          total=acc.sums.total,
          first=acc.first,
          last=acc.last,
          fallback_calls=acc.fallback_calls,
          fallback_output=acc.fallback_output,
          accounts=sorted(
              (
                  LedgerAccount(name=name, calls=s.calls, output=s.output, total=s.total)
                  for name, s in acc.accounts.items()),
              key=lambda a: (-a.total, a.name)),
      )
      for (source, model), acc in accs.items()
  ]
  rows.sort(key=lambda r: (-r.total, r.source, r.model))
  return rows


def _native_starts_from_accs(accs: dict[tuple[str, str], _ModelSum]) -> dict[str, str]:
  """Each source's earliest native day across its groups; fallback-only sources are absent."""
  starts: dict[str, str] = {}
  for (source, _model), acc in accs.items():
    if acc.native_first and (source not in starts or acc.native_first < starts[source]):
      starts[source] = acc.native_first
  return starts


@dataclass
class _RowsMemo:
  """The page-rows read's served state, one entry for the one production ledger path."""

  path: str
  identity: tuple[int, int]
  generation: int
  rows: list[LedgerRow]
  native_starts: dict[str, str]
  accs: dict[tuple[str, str], _ModelSum]
  rowcount: int
  excluded_fallback: frozenset[str]
  rewrite_epoch: int


def default_ledger_path() -> Path:
  """The CLI's default ledger: under the charliebot home, beside the sessions it indexes."""
  from src.core.config import get_config

  return get_config().charliebot_home / "usage" / "ledger.sqlite3"


class UsageLedger:
  """SQLite store behind the /token-usage page; see the module docstring.

  The schema is created on open (IF NOT EXISTS), so a fresh path yields an empty
  ledger and an existing one keeps every row. The one deletion of a usage row is
  ``record_file``'s ``supersede_prefix`` range: a re-parse superseding its own source's
  legacy-id records. A ledger with no ``schema`` stamp in ``ledger_meta`` upgrades to
  '2' once on open (``_upgrade_schema``). A ledger whose stored aggregate-trigger bodies differ from this
  module's gets the five swapped and a backfilled aggregate re-priced on open
  (``_refresh_agg_triggers``).
  """

  # The page-rows memo, one entry for the one production ledger path: the rows
  # serve repeat reads unchanged while the file sits byte-still. Validity rides
  # three witnesses together -- the file's (size, mtime_ns) pair, this process's
  # write generation, and the journal mode -- because each alone has a hole the
  # next one covers:
  #   * the stat pair moves on every committing writer's main-db rewrite (the
  #     ledger runs the default rollback journal, never WAL, whose commits hide
  #     in the -wal sidecar), but a forged pair equal to the stored one defeats it;
  #   * the generation catches this process's own writes outright;
  #   * a later journal-mode flip to WAL would silence future stat movement, so a
  #     memo is stored and served only under a non-WAL mode.
  # The (size, mtime_ns) gate is the same witness class the capture gates store
  # (``capture_gates``), and an mtime_ns collision across two real writes is the
  # accepted risk those gates already carry.
  #
  # A miss whose only change is this process's own inserts extends the memo by the
  # row delta (``_try_fold_rows``) instead of re-running the full grouped pass --
  # the page's own capture makes that the common shape under active turns. Any
  # other change falls back to the full pass: an in-place value rewrite or a
  # superseding prefix delete bumps the ledger's rewrite epoch
  # (``_rewrite_epoch``), which the memo stores and the fold compares -- the
  # delete must bump it because N deletes paired with N inserts slips past the
  # row-count witness; a re-captured record upserts the same values and bumps
  # nothing; a foreign writer's inserts and deletions fail the row-count
  # witness; WAL silences the stat pair and stores no memo.
  _rows_memo: _RowsMemo | None = None
  _write_generation: int = 0

  def __init__(self, path: Path) -> None:
    self._inserted_ids: set[str] = set()
    self._path = Path(path)
    self._path.parent.mkdir(parents=True, exist_ok=True)
    self._conn = sqlite3.connect(self._path, timeout=USAGE_LEDGER_LOCK_WAIT_SECONDS)
    self._conn.row_factory = sqlite3.Row
    self._conn.executescript(_SCHEMA)
    stamp = self._conn.execute("SELECT value FROM ledger_meta WHERE key = 'schema'").fetchone()
    if stamp is None or stamp["value"] != "2":
      self._upgrade_schema()  # recreates the aggregate triggers inside its transaction
    else:
      self._refresh_agg_triggers()  # one transaction; a no-op when the stored bodies match
    self._conn.commit()

  def _upgrade_schema(self) -> None:
    """The one-time upgrade to schema '2', one transaction: add ``in_unsplit`` to both
    tables, swap the five token-summing aggregate triggers for bodies that carry it, move
    the gone-source records' input, re-price a backfilled aggregate from the table
    wholesale, bump the rewrite epoch, and stamp.

    The triggers are dropped and recreated before the move so the update trigger
    subtracts and re-adds every moved row's aggregate contribution; the re-add lands on
    its (group, day) row (``_agg_contribute_sql``), and the wholesale replacement
    (``_AGG_WIPE_SQL`` then ``_AGG_BACKFILL_SQL``) re-prices a backfilled aggregate from
    the table, so after the move the aggregate equals the table pass again. A
    never-backfilled aggregate is not served yet -- its one-time backfill prices the same
    ground truth when it runs. Only records whose source file is gone move: a record whose file
    remains is re-parsed with the correct split, and an old-code process rewriting its
    row would put the input back into ``in_fresh`` beside a moved ``in_unsplit`` and
    count the same input twice. BEGIN IMMEDIATE re-checks the stamp under the write
    lock, so two processes opening an un-stamped ledger upgrade once.
    """
    self._conn.execute("BEGIN IMMEDIATE")
    try:
      stamp = self._conn.execute("SELECT value FROM ledger_meta WHERE key = 'schema'").fetchone()
      if stamp is not None and stamp["value"] == "2":
        self._conn.rollback()  # another process upgraded while this one waited on the lock
        return
      for table in ("usage", "usage_agg"):
        columns = {row["name"] for row in self._conn.execute(f"PRAGMA table_info({table})")}
        if "in_unsplit" not in columns:
          self._conn.execute(f"ALTER TABLE {table} ADD COLUMN in_unsplit INTEGER NOT NULL DEFAULT 0")
      for name in _AGG_SUM_TRIGGER_NAMES:
        self._conn.execute(f"DROP TRIGGER IF EXISTS {name}")
      for statement in _AGG_TRIGGER_STATEMENTS:
        self._conn.execute(statement)
      origins = [
          row["origin"]
          for row in self._conn.execute(f"SELECT DISTINCT origin FROM usage WHERE {_UPGRADE_MOVE_RECORDS_SQL}")
      ]
      gone = [origin for origin in origins if not origin or not Path(origin).exists()]
      for start in range(0, len(gone), 900):  # SQLite's host-parameter ceiling
        chunk = gone[start:start + 900]
        marks = ",".join("?" * len(chunk))
        self._conn.execute(
            f"""UPDATE usage SET in_unsplit = in_fresh, in_fresh = 0
            WHERE {_UPGRADE_MOVE_RECORDS_SQL} AND origin IN ({marks})""", chunk)
      if self._conn.execute("SELECT 1 FROM ledger_meta WHERE key = 'agg_backfilled'").fetchone() is not None:
        self._conn.execute(_AGG_WIPE_SQL)  # replace the served aggregate wholesale (see the docstring)
        self._conn.execute(_AGG_BACKFILL_SQL)
      self._conn.execute(
          "INSERT INTO ledger_meta (key, value) VALUES ('rewrite_epoch', '1')"
          " ON CONFLICT(key) DO UPDATE SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT)")
      self._conn.execute(
          "INSERT INTO ledger_meta (key, value) VALUES ('schema', '2')"
          " ON CONFLICT(key) DO UPDATE SET value = '2'")
      self._conn.commit()
    except BaseException:
      self._conn.rollback()
      raise

  def _refresh_agg_triggers(self) -> None:
    """Swap this module's aggregate trigger bodies in when a stored one differs, then
    re-price a backfilled aggregate from the table -- one transaction.

    CREATE TRIGGER IF NOT EXISTS never replaces an existing trigger, so every existing
    ledger runs the trigger bodies its creating release wrote. A pre-fix ledger's add arm
    skipped any (group, day) row that already existed, dropping every later write onto a
    served group-day and every rewrite's re-added contribution, so the page the backfilled
    aggregate serves undercounts and shrinks on rewrites. Comparing each stored trigger's
    sqlite_master.sql against this module's (``_AGG_TRIGGER_SQLS``) catches that drift on
    open: the five triggers are dropped and recreated, and the wholesale replacement
    rebuilds the aggregate from the table under the fresh bodies. A never-backfilled
    aggregate is not served yet -- its one-time backfill prices the same ground truth when
    it runs. A ledger whose stored bodies match this module's runs nothing.
    """
    self._conn.execute("BEGIN IMMEDIATE")
    try:
      stored = {
          row["name"]: row["sql"]
          for row in self._conn.execute("SELECT name, sql FROM sqlite_master WHERE type = 'trigger'")
      }
      if all(stored.get(name) == expected for name, expected in _AGG_TRIGGER_SQLS.items()):
        self._conn.rollback()  # the stored bodies already match this module's
        return
      for name in _AGG_SUM_TRIGGER_NAMES:
        self._conn.execute(f"DROP TRIGGER IF EXISTS {name}")
      for statement in _AGG_TRIGGER_STATEMENTS:
        self._conn.execute(statement)
      if self._conn.execute("SELECT 1 FROM ledger_meta WHERE key = 'agg_backfilled'").fetchone() is not None:
        self._conn.execute(_AGG_WIPE_SQL)  # re-price the served aggregate under the fresh bodies
        self._conn.execute(_AGG_BACKFILL_SQL)
      self._conn.commit()
    except BaseException:
      self._conn.rollback()
      raise

  def __enter__(self) -> Self:
    return self

  def __exit__(self, exc_type: object, exc: object, tb: object) -> None:
    self.close()

  def close(self) -> None:
    self._conn.close()

  def captured_sigs(self, host: str) -> dict[str, str]:
    """Every captured file path and its last-recorded signature for one host."""
    rows = self._conn.execute("SELECT path, sig FROM captured_files WHERE host = ?", (host,)).fetchall()
    return {row["path"]: row["sig"] for row in rows}

  def captured_sig(self, host: str, path: str) -> str | None:
    """One captured file path's last-recorded signature for *host*, or None when uncaptured."""
    row = self._conn.execute("SELECT sig FROM captured_files WHERE host = ? AND path = ?", (host, path)).fetchone()
    return None if row is None else row["sig"]

  def captured_gate(self, host: str, path: str) -> tuple[tuple[tuple[int, int], tuple[int, int] | None], str] | None:
    """One stored probe gate: the file-state pairs the probe ran under and the signature it
    computed, or None when nothing is stored for the (host, path)."""
    row = self._conn.execute(
        "SELECT main_size, main_mtime_ns, wal_size, wal_mtime_ns, sig FROM capture_gates"
        " WHERE host = ? AND path = ?", (host, path)).fetchone()
    if row is None:
      return None
    wal = None if row["wal_size"] is None else (row["wal_size"], row["wal_mtime_ns"])
    return ((row["main_size"], row["main_mtime_ns"]), wal), row["sig"]

  def record_gate(self, host: str, path: str, main: tuple[int, int], wal: tuple[int, int] | None, sig: str) -> None:
    """Store one probe's gate: the file-state pairs it ran under and the signature it computed."""
    with self._conn:
      self._conn.execute(
          """INSERT INTO capture_gates (host, path, main_size, main_mtime_ns, wal_size, wal_mtime_ns, sig)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(host, path) DO UPDATE SET
               main_size = excluded.main_size, main_mtime_ns = excluded.main_mtime_ns,
               wal_size = excluded.wal_size, wal_mtime_ns = excluded.wal_mtime_ns,
               sig = excluded.sig""",
          (host, path, main[0], main[1], None if wal is None else wal[0], None if wal is None else wal[1], sig))
    UsageLedger._write_generation += 1

  def record_file(self, host: str, path: str, sig: str, records: Sequence[UsageRecord],
                  supersede_prefix: str | None = None) -> int:
    """Store one file capture atomically and return the record count.

    Every record is upserted on ``record_id`` (the latest capture wins on every column
    but ``ts``, which keeps the earlier non-empty value -- a re-capture of the same call
    may carry a later stamp, and the call's own time is the earlier one), each NATIVE
    record's sessions are registered once (first registration wins, later re-captures
    keep them), each FALLBACK record's sessions are linked, and the file's signature is
    remembered for the collector's skip decision.

    With ``supersede_prefix``, the same transaction first deletes every usage row whose
    ``record_id`` starts with it -- a primary-key range, not LIKE -- together with those
    rows' ``fallback_sessions`` rows, before the upserts land: the ledger's one bounded
    deletion path, a re-parse superseding a source file's legacy-id records. A delete
    that removed at least one row bumps the rewrite epoch, because N deletes paired with
    N inserts slips past the fold's row-count witness.
    """
    captured_at = datetime.now(UTC).isoformat()
    # Two witnesses ride every write, and the row-delta fold consumes both. (1) The
    # inserted ids: an upsert that lands on an existing record_id updates that row in
    # place, so only the ids absent here are new rows -- the fold's delta. (2) The
    # value witness: an upsert that changes an existing row's aggregated values
    # invalidates every aggregate built before it, and no delta can see the change --
    # the fold's poison. A re-captured record upserts the same values (the parse is
    # deterministic), so the witness stamps the poison only on a real change. The
    # existing rows come back batched, 900 ids per read, not one query per record:
    # the capture's wall is the page's wall.
    existing: dict[str, tuple] = {}
    ids = [rec.record_id for rec in records]
    for start in range(0, len(ids), 900):  # SQLite's host-parameter ceiling
      chunk = ids[start:start + 900]
      marks = ",".join("?" * len(chunk))
      for row in self._conn.execute(
          f"SELECT record_id, kind, source, model, account, ts, in_fresh, cache_write, cache_read,"
          f" in_unsplit, output FROM usage WHERE record_id IN ({marks})", chunk):
        existing[row["record_id"]] = tuple(row)[1:]
    with self._conn:
      deleted = 0
      if supersede_prefix is not None:
        # A primary-key range: the upper bound is the prefix with its last character
        # incremented, so 'codex:t1:' matches 'codex:t1:0' but not 'codex:t10:0' or
        # 'codex:t1x:0'. The usage rows go first, while their fallback_sessions links
        # still hold -- the delete trigger's counted-row check reads them, and an
        # excluded row must not be subtracted.
        upper = supersede_prefix[:-1] + chr(ord(supersede_prefix[-1]) + 1)
        deleted = self._conn.execute(
            "DELETE FROM usage WHERE record_id >= ? AND record_id < ?", (supersede_prefix, upper)).rowcount
        self._conn.execute(
            "DELETE FROM fallback_sessions WHERE record_id >= ? AND record_id < ?", (supersede_prefix, upper))
      rewrote = deleted > 0
      for rec in records:
        ts = rec.ts
        old_values = existing.get(rec.record_id)
        if old_values is None:
          self._inserted_ids.add(rec.record_id)
        else:
          old_ts = old_values[4]
          # The row keeps the earlier non-empty ts: an existing ts stays when the
          # incoming one is empty or later; otherwise the incoming one wins.
          if old_ts != "" and (ts == "" or ts > old_ts):
            ts = old_ts
          # The witness compares the values the row holds after the upsert, with the
          # kept ts, so a re-capture differing only by a later stamp is not a rewrite.
          if old_values != (rec.kind.value, rec.source, rec.model, rec.account, ts, rec.in_fresh, rec.cache_write,
                            rec.cache_read, rec.in_unsplit, rec.output):
            rewrote = True
        # The sessions register before the usage row: the aggregate's insert trigger
        # counts a fallback row only while none of its sessions is native, and that
        # check reads this record's own registrations.
        if rec.kind == RecordKind.NATIVE:
          for session in rec.sessions:
            self._conn.execute(
                "INSERT OR IGNORE INTO native_sessions (session, source) VALUES (?, ?)", (session, rec.source))
        else:
          for session in rec.sessions:
            self._conn.execute(
                "INSERT OR IGNORE INTO fallback_sessions (record_id, session) VALUES (?, ?)", (rec.record_id, session))
        self._conn.execute(
            _UPSERT_USAGE_SQL, (
                rec.record_id, rec.kind.value, rec.source, rec.model, rec.account, host, ts, rec.in_fresh,
                rec.cache_write, rec.cache_read, rec.in_unsplit, rec.output, path, captured_at))
      if rewrote:
        # A value rewrite invalidates every aggregate built before it in any process,
        # and a superseding delete can pair N deletes with N inserts the fold's
        # row-count witness cannot see -- the witness lives in the database, not in
        # this process.
        self._conn.execute(
            "INSERT INTO ledger_meta (key, value) VALUES ('rewrite_epoch', '1')"
            " ON CONFLICT(key) DO UPDATE SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT)")
      self._conn.execute(
          """INSERT INTO captured_files (host, path, sig) VALUES (?, ?, ?)
             ON CONFLICT(host, path) DO UPDATE SET sig = excluded.sig""", (host, path, sig))
    UsageLedger._write_generation += 1
    return len(records)

  def model_rows(self) -> list[LedgerRow]:
    """The /token-usage page rows, aggregated from counted records only."""
    return self.model_rows_with_native_starts()[0]

  def _file_stat_identity(self) -> tuple[int, int]:
    """The ledger file's (size, mtime_ns) pair."""
    st = self._path.stat()
    return (st.st_size, st.st_mtime_ns)

  def _rewrite_epoch(self) -> int:
    """The ledger's in-place value-rewrite counter, one row in ledger_meta.

    The counter lives in the database, not in the process: a foreign writer's
    value rewrite must poison a memo this process built, and no process-local
    witness can see it.
    """
    row = self._conn.execute("SELECT value FROM ledger_meta WHERE key = 'rewrite_epoch'").fetchone()
    return 0 if row is None else int(row["value"])

  def _journal_mode(self) -> str:
    return self._conn.execute("PRAGMA journal_mode").fetchone()[0]

  def model_rows_with_native_starts(self) -> tuple[list[LedgerRow], dict[str, str]]:
    """The page rows plus each source's first native day, served from the fastest
    exact layer.

    Repeat reads while the ledger file sits byte-still since the last read are
    served from the class-level memo (see its comment for the validity
    witnesses). A memo miss caused by this process's own inserts extends the memo
    by the row delta instead (``_try_fold_rows``). Any other miss serves the
    trigger-maintained aggregate (``_agg_accs``) once its one-time backfill has
    run; the grouped table pass prices the read only before that backfill.

    Rows and accounts are both sorted by total descending, with the group keys as
    tiebreakers so the same ledger content always yields the same ordering.

    The native start is the earliest dated day across the source's NATIVE groups.
    Fallback-only sources are absent: their spans ride on CLI logs that may be
    pruned, so they cannot anchor a source's history. The day rides the group's
    NULLIF'd ``first``: an empty-ts native row must not MIN the source to the
    empty string the way a raw ``MIN(SUBSTR(ts, 1, 10))`` over the table did.
    """
    identity = self._file_stat_identity()
    generation = UsageLedger._write_generation
    memo = UsageLedger._rows_memo
    wal = self._journal_mode() == "wal"
    if (memo is not None and memo.path == str(self._path) and memo.identity == identity and
        memo.generation == generation and not wal):
      return memo.rows, memo.native_starts
    if memo is not None and memo.path == str(self._path) and not wal:
      folded = self._try_fold_rows(memo, identity, generation)
      if folded is not None:
        return folded
    self._inserted_ids.clear()
    accs = self._agg_accs()
    # Re-anchor after the aggregate call: the one-time backfill it may run is this
    # read's own write, and the rows below reflect the state it leaves -- only a
    # writer after this point may void the memo.
    identity = self._file_stat_identity()
    if accs is None:
      # The aggregate has not been backfilled: the table pass below covers every
      # insert this instance tracked, tracked or not.
      accs = {}
      for row in self._conn.execute(_MODEL_ROWS_SQL):
        # The grouped rows enumerate every kind stored (retirement only ever drops
        # kind='fallback' rows): an unknown stored value raises on this read's own
        # pass instead of surfacing silently dropped from the page.
        _fold_grouped_row(accs, row)
    rows = _rows_from_accs(accs)
    native_starts = _native_starts_from_accs(accs)
    # Store only a read the file provably covered: the pre-read stat must survive
    # to the post-read check (a writer in between would leave the read consistent
    # with the pre-write state), and never under WAL, whose commits the stat pair
    # cannot see.
    if (generation == UsageLedger._write_generation and self._file_stat_identity() == identity and
        self._journal_mode() != "wal"):
      excluded = frozenset(row["record_id"] for row in self._conn.execute(_EXCLUDED_FALLBACK_IDS_SQL))
      rowcount = self._conn.execute("SELECT COUNT(*) FROM usage").fetchone()[0]
      UsageLedger._rows_memo = _RowsMemo(
          path=str(self._path),
          identity=identity,
          generation=generation,
          rows=rows,
          native_starts=native_starts,
          accs=accs,
          rowcount=rowcount,
          excluded_fallback=excluded,
          rewrite_epoch=self._rewrite_epoch())
    return rows, native_starts

  def _agg_accs(self) -> dict[tuple[str, str], _ModelSum] | None:
    """The page rows' accumulators served from the trigger-maintained aggregate, or None
    while the aggregate has not been backfilled (the caller prices the table instead, and
    the next read serves from it).

    The triggers fire for every writer -- the schema carries them, so any process's
    capture maintains the aggregate whatever code version it runs -- so a backfilled
    aggregate needs no per-read freshness witness. A dropped aggregate table (the schema's
    own statements never drop) shows up as a ready flag over an empty aggregate while the
    table holds rows, and the backfill below re-prices it.
    """
    ready = self._conn.execute("SELECT value FROM ledger_meta WHERE key = 'agg_backfilled'").fetchone()
    empty = self._conn.execute("SELECT 1 FROM usage_agg LIMIT 1").fetchone() is None
    if ready is not None and not (empty and self._conn.execute("SELECT 1 FROM usage LIMIT 1").fetchone()):
      accs: dict[tuple[str, str], _ModelSum] = {}
      for row in self._conn.execute(_AGG_ROWS_SQL):
        _fold_grouped_row(accs, row)
      return accs
    self._backfill_agg()
    return None

  def _backfill_agg(self) -> None:
    """Replace the aggregate from the table once, under the write lock.

    BEGIN IMMEDIATE serializes the check-plus-backfill across processes: two first
    readers cannot both backfill, and a concurrent record_file waits out the one pass
    instead of interleaving with it. The replacement is wholesale -- every row the
    aggregate carries goes away with the delete, so increments the triggers wrote before
    the pass never survive beside it. A failure rolls the flag back with the rows, so
    the next read retries.
    """
    self._conn.execute("BEGIN IMMEDIATE")
    try:
      ready = self._conn.execute("SELECT value FROM ledger_meta WHERE key = 'agg_backfilled'").fetchone()
      if ready is None:
        self._conn.execute(_AGG_WIPE_SQL)
        self._conn.execute(_AGG_BACKFILL_SQL)
        self._conn.execute(
            "INSERT INTO ledger_meta (key, value) VALUES ('agg_backfilled', '1')"
            " ON CONFLICT(key) DO UPDATE SET value = '1'")
      self._conn.commit()
    except BaseException:
      self._conn.rollback()
      raise

  def _try_fold_rows(self, memo: _RowsMemo, identity: tuple[int, int],
                     generation: int) -> tuple[list[LedgerRow], dict[str, str]] | None:
    """Extend the memo's aggregates by the rows this process inserted since it was built.

    Returns the folded rows, or None when any fold witness fails -- the caller
    then re-runs the full pass, which is correct against every ledger state. The
    delta reads only the tracked inserted ids, so its cost scales with what the
    capture wrote, not with the table. A foreign writer's inserts are invisible
    to this process's tracking, so the row-count witness refuses the fold and
    the full pass prices them.
    """
    if generation == memo.generation or self._rewrite_epoch() != memo.rewrite_epoch:
      return None
    rowcount = self._conn.execute("SELECT COUNT(*) FROM usage").fetchone()[0]
    if rowcount != memo.rowcount + len(self._inserted_ids):
      return None  # rows moved under us beyond this instance's own inserts
    delta: list[sqlite3.Row] = []
    ids = sorted(self._inserted_ids)
    for start in range(0, len(ids), 900):  # SQLite's host-parameter ceiling
      chunk = ids[start:start + 900]
      marks = ",".join("?" * len(chunk))
      delta.extend(
          self._conn.execute(
              f"""SELECT source, model, account, kind, ts, in_fresh, cache_write, cache_read, in_unsplit, output
              FROM usage u WHERE u.record_id IN ({marks}) AND NOT EXISTS (
                SELECT 1 FROM fallback_sessions fs JOIN native_sessions ns ON ns.session = fs.session
                WHERE u.kind = 'fallback' AND fs.record_id = u.record_id)""", chunk))
    accs = memo.accs
    for row in delta:
      _fold_raw_row(accs, row)
    if any(row["kind"] == RecordKind.NATIVE.value for row in delta):
      # A new native session can retire a fallback row the memo counted; the
      # touched groups cannot be patched incrementally, so the fold stands down
      # and the full pass re-aggregates them.
      excluded_now = frozenset(row["record_id"] for row in self._conn.execute(_EXCLUDED_FALLBACK_IDS_SQL))
      if excluded_now != memo.excluded_fallback:
        return None
    else:
      excluded_now = memo.excluded_fallback
    rows = _rows_from_accs(accs)
    native_starts = _native_starts_from_accs(accs)
    # The same coverage rule the full pass stores under: the fold's reads must
    # have seen exactly the state between the memo and now, with no writer since.
    if (generation != UsageLedger._write_generation or self._file_stat_identity() != identity or
        self._journal_mode() == "wal"):
      return None
    UsageLedger._rows_memo = _RowsMemo(
        path=memo.path,
        identity=identity,
        generation=generation,
        rows=rows,
        native_starts=native_starts,
        accs=accs,
        rowcount=rowcount,
        excluded_fallback=excluded_now,
        rewrite_epoch=memo.rewrite_epoch)
    self._inserted_ids.clear()
    return rows, native_starts
