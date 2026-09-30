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
query-time rule over the registered sessions, not a deletion — the module
contains no DELETE statement, so re-reading a pruned-away file's captured rows
and their spans stays possible forever.

``captured_files`` remembers the last signature written per (host, path), so a
collector can skip files it already ingested by content, not by existence.

``capture_gates`` remembers one source's *probe* per (host, path): the file-state
pairs a probe ran under and the signature it computed, so a fresh process reuses
that probe while the files sit byte-still instead of re-scanning the source.
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
  output INTEGER NOT NULL,
  origin TEXT NOT NULL,
  captured_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS usage_group_cover ON usage(kind, source, model, account, ts, in_fresh, cache_write, cache_read, output);
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
"""

# The usage row is upserted whole on a repeated record_id (the same API call seen again
# from another file or host): the latest capture wins on every non-key column.
_UPSERT_USAGE_SQL = """
INSERT INTO usage (record_id, kind, source, model, account, host, ts,
                   in_fresh, cache_write, cache_read, output, origin, captured_at)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
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

  ``fallback_calls``/``fallback_output`` cover only the counted FALLBACK rows behind
  this row — the part of the row that could still change if a pruned CLI log
  resurfaces and retires the fallbacks.
  """

  source: str
  model: str
  in_fresh: int
  cache_write: int
  cache_read: int
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
  """Mutable token accumulator for one bucket; ``total`` is the four token fields.

  ``add`` folds one aggregate group row, whose ``calls`` column carries the group's
  record count.
  """

  calls: int = 0
  in_fresh: int = 0
  cache_write: int = 0
  cache_read: int = 0
  output: int = 0

  def add(self, row: sqlite3.Row) -> None:
    self.calls += row["calls"]
    self.in_fresh += row["in_fresh"]
    self.cache_write += row["cache_write"]
    self.cache_read += row["cache_read"]
    self.output += row["output"]

  @property
  def total(self) -> int:
    return self.in_fresh + self.cache_write + self.cache_read + self.output


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
      k: row[k] for k in ("source", "model", "account", "kind", "in_fresh", "cache_write", "cache_read", "output")
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
  ledger and an existing one keeps every row. No statement here deletes.
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
  # other change falls back to the full pass: an in-place value rewrite bumps the
  # ledger's rewrite epoch (``_rewrite_epoch``), which the memo stores and the
  # fold compares -- a re-captured record upserts the same values and bumps
  # nothing; a foreign writer's inserts and every deletion fail the row-count
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
    self._conn.commit()

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

  def record_file(self, host: str, path: str, sig: str, records: Sequence[UsageRecord]) -> int:
    """Store one file capture atomically and return the record count.

    Every record is upserted on ``record_id`` (latest capture wins, ``origin`` and
    ``captured_at`` stamped from this write), each NATIVE record's sessions are
    registered once (first registration wins, later re-captures keep them), each
    FALLBACK record's sessions are linked, and the file's signature is remembered
    for the collector's skip decision.
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
          f" output FROM usage WHERE record_id IN ({marks})", chunk):
        existing[row["record_id"]] = tuple(row)[1:]
    rewrote = False
    with self._conn:
      for rec in records:
        new_values = (
            rec.kind.value, rec.source, rec.model, rec.account, rec.ts, rec.in_fresh, rec.cache_write, rec.cache_read,
            rec.output)
        old_values = existing.get(rec.record_id)
        if old_values is None:
          self._inserted_ids.add(rec.record_id)
        elif old_values != new_values:
          rewrote = True
        self._conn.execute(
            _UPSERT_USAGE_SQL, (
                rec.record_id, rec.kind.value, rec.source, rec.model, rec.account, host, rec.ts, rec.in_fresh,
                rec.cache_write, rec.cache_read, rec.output, path, captured_at))
        if rec.kind == RecordKind.NATIVE:
          for session in rec.sessions:
            self._conn.execute(
                "INSERT OR IGNORE INTO native_sessions (session, source) VALUES (?, ?)", (session, rec.source))
        else:
          for session in rec.sessions:
            self._conn.execute(
                "INSERT OR IGNORE INTO fallback_sessions (record_id, session) VALUES (?, ?)", (rec.record_id, session))
      if rewrote:
        # A value rewrite invalidates every aggregate built before it in any process,
        # so the witness lives in the database, not in this process.
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
    """The page rows plus each source's first native day, from one grouped pass.

    Repeat reads while the ledger file sits byte-still since the last read are
    served from the class-level memo (see its comment for the validity
    witnesses); the grouped pass below runs only on a memo miss. A miss caused
    by this process's own inserts extends the memo by the row delta instead
    (``_try_fold_rows``); the full pass is the fallback for every other miss.

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
    # The full pass below covers every insert this instance tracked, tracked or not.
    self._inserted_ids.clear()
    accs: dict[tuple[str, str], _ModelSum] = {}
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
              f"""SELECT source, model, account, kind, ts, in_fresh, cache_write, cache_read, output
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
