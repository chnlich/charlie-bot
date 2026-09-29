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
"""

from __future__ import annotations

import sqlite3
from collections.abc import Sequence
from dataclasses import dataclass, field
from datetime import UTC, datetime
from enum import StrEnum
from pathlib import Path
from typing import Self

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
# sessions has a NATIVE record (the any-match EXISTS join). An unknown stored kind
# matches neither arm — and is rejected at read time by the RecordKind conversion.
_COUNTED_WHERE_SQL = """
WHERE u.kind = 'native'
   OR (u.kind = 'fallback'
       AND NOT EXISTS (
         SELECT 1 FROM fallback_sessions fs
         WHERE fs.record_id = u.record_id
           AND EXISTS (SELECT 1 FROM native_sessions ns WHERE ns.session = fs.session)))
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
  """Per-(source, model) accumulator: every counted row plus the fallback split."""

  sums: _Sum = field(default_factory=_Sum)
  fallback_calls: int = 0
  fallback_output: int = 0
  first: str = ""
  last: str = ""
  accounts: dict[str, _Sum] = field(default_factory=dict)


def default_ledger_path() -> Path:
  """The CLI's default ledger: under the charliebot home, beside the sessions it indexes."""
  from src.core.config import get_config

  return get_config().charliebot_home / "usage" / "ledger.sqlite3"


class UsageLedger:
  """SQLite store behind the /token-usage page; see the module docstring.

  The schema is created on open (IF NOT EXISTS), so a fresh path yields an empty
  ledger and an existing one keeps every row. No statement here deletes.
  """

  def __init__(self, path: Path) -> None:
    self._path = Path(path)
    self._path.parent.mkdir(parents=True, exist_ok=True)
    self._conn = sqlite3.connect(self._path, timeout=30)
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

  def record_file(self, host: str, path: str, sig: str, records: Sequence[UsageRecord]) -> int:
    """Store one file capture atomically and return the record count.

    Every record is upserted on ``record_id`` (latest capture wins, ``origin`` and
    ``captured_at`` stamped from this write), each NATIVE record's sessions are
    registered once (first registration wins, later re-captures keep them), each
    FALLBACK record's sessions are linked, and the file's signature is remembered
    for the collector's skip decision.
    """
    captured_at = datetime.now(UTC).isoformat()
    with self._conn:
      for rec in records:
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
      self._conn.execute(
          """INSERT INTO captured_files (host, path, sig) VALUES (?, ?, ?)
             ON CONFLICT(host, path) DO UPDATE SET sig = excluded.sig""", (host, path, sig))
    return len(records)

  def model_rows(self) -> list[LedgerRow]:
    """The /token-usage page rows, aggregated from counted records only.

    Rows and accounts are both sorted by total descending, with the group keys as
    tiebreakers so the same ledger content always yields the same ordering.
    """
    # Read every stored kind through the enum first: the counted-rows query filters on
    # kind in SQL, so an unknown stored value would otherwise be silently dropped from
    # the page instead of surfacing as the read error it is.
    for row in self._conn.execute("SELECT DISTINCT kind FROM usage"):
      RecordKind(row["kind"])
    accs: dict[tuple[str, str], _ModelSum] = {}
    for row in self._conn.execute(_MODEL_ROWS_SQL):
      kind = RecordKind(row["kind"])
      acc = accs.setdefault((row["source"], row["model"]), _ModelSum())
      acc.sums.add(row)
      first, last = row["first"], row["last"]  # NULL when the group has no dated ts
      if first and (not acc.first or first < acc.first):
        acc.first = first
      if last and (not acc.last or last > acc.last):
        acc.last = last
      if kind == RecordKind.FALLBACK:
        acc.fallback_calls += row["calls"]
        acc.fallback_output += row["output"]
      acc.accounts.setdefault(row["account"], _Sum()).add(row)
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

  def native_start(self) -> dict[str, str]:
    """First day (``ts[:10]``) a NATIVE record was seen, per source.

    Fallback-only sources are absent: their spans ride on CLI logs that may be
    pruned, so they cannot anchor a source's history.
    """
    rows = self._conn.execute(
        "SELECT source, MIN(SUBSTR(ts, 1, 10)) AS start FROM usage"
        " WHERE kind = 'native' GROUP BY source").fetchall()
    return {row["source"]: row["start"] for row in rows}
