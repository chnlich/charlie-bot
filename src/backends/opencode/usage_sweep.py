"""opencode's part of the cold-storage sweep (src/features/usage/storage_cool.py): the event store.

``usage_logs.sweep()`` imports this module on its first call, so a usage capture and
``charliebot --help`` never load it. The sweep's one deletion rule is stated on ``SweepScope``
(src/runtime/hooks/usage_sources.py); an aggregate id of the database is the backend session id that
CharlieBot metadata references.

The store loses ``event`` rows in byte-capped delete transactions, then the ``event_sequence`` row,
whose foreign-key cascade picks up any stragglers. ``session``, ``message`` and ``part`` stay, and
the usage log reads ``message``.

The sweep never compacts on its own: VACUUM is the manual ``--vacuum`` option, which refuses while an
``opencode serve`` writer is alive (``--force`` overrides). The report carries the freelist either way.
"""

import os
import sqlite3
import sys
import time
from pathlib import Path

from src.infra import home, log_once
from src.infra.timeouts import SQLITE_LOCK_WAIT_MS, SQLITE_LOCK_WAIT_SECONDS
from src.runtime.hooks import usage_sources

log = log_once.LazyStructlogLogger()

_SQL_PARAM_CHUNK = 500

# opencode event deletion caps: every delete transaction stays within both the
# byte cap (summed ``length(data)``) and the row cap, and the loop yields between
# transactions so a live opencode writer interleaves instead of starving.
_CHUNK_MAX_BYTES = 32 << 20
_CHUNK_MAX_ROWS = 1000
_CHUNK_YIELD_SECONDS = 0.025

_AGGREGATE_IDS_SQL = "select aggregate_id from event_sequence"
_AGGREGATE_SIZES_SQL = (
    "select aggregate_id, count(*), sum(length(data)) from event where aggregate_id in ({}) group by aggregate_id")
_SESSION_UPDATED_SQL = "select id, time_updated from session"


def _opencode_query(db: Path, sql: str, parameters: tuple = ()) -> list[tuple] | None:
  """One read-only query against the opencode store; None on failure, [] on an empty result."""
  try:
    connection = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    try:
      return connection.execute(sql, parameters).fetchall()
    finally:
      connection.close()
  except sqlite3.Error as e:
    log.warning("storage_cool_opencode_scan_failed", db=str(db), error=str(e))
    return None


def _opencode_candidate_ids(db: Path, backend_session: str | None) -> list[str] | None:
  """The aggregate ids a sweep may delete: all of them, or the scoped one."""
  if backend_session is None:
    rows = _opencode_query(db, _AGGREGATE_IDS_SQL)
  else:
    rows = _opencode_query(db, f"{_AGGREGATE_IDS_SQL} where aggregate_id = ?", (backend_session,))
  return [str(row[0]) for row in rows] if rows is not None else None


def _opencode_aggregate_sizes(db: Path, aggregate_ids: list[str]) -> dict[str, tuple[int, int]] | None:
  """Event counts and byte totals for *aggregate_ids*, from index-driven lookups.

  One failed query drops the whole map, so a partially-read scan never reports or
  deletes bytes it did not see; the next run re-reads and finishes.
  """
  # Keep aggregates with no event rows in the map too.  Their sequence row is
  # still a backend record covered by the deletion rule, even though it frees
  # zero event bytes.
  sizes: dict[str, tuple[int, int]] = dict.fromkeys(aggregate_ids, (0, 0))
  for start in range(0, len(aggregate_ids), _SQL_PARAM_CHUNK):
    chunk = aggregate_ids[start:start + _SQL_PARAM_CHUNK]
    rows = _opencode_query(db, _AGGREGATE_SIZES_SQL.format(",".join("?" * len(chunk))), tuple(chunk))
    if rows is None:
      return None
    for aggregate_id, count, size in rows:
      sizes[str(aggregate_id)] = (int(count), int(size or 0))
  return sizes


def _opencode_session_updated(db: Path) -> dict[str, int] | None:
  """The backend's own last-update timestamp (ms epoch) per aggregate id."""
  rows = _opencode_query(db, _SESSION_UPDATED_SQL)
  if rows is None:
    return None
  return {str(row[0]): int(row[1]) for row in rows if row[1] is not None}


def _opencode_targets(db: Path, scope: usage_sources.SweepScope) -> dict[str, tuple[int, int]] | None:
  """Aggregate ids the rule deletes, with their event counts and byte totals.

  In a scoped run the named session's own aggregate is the only candidate (the
  cold rule is already established for it); otherwise a referenced aggregate goes
  when every referencing session is cold, and an unreferenced one once the
  backend's own timestamp has been idle past the safety window.
  """
  scoped_backends = scope.scoped_backend_sessions()
  if scope.session_id is not None and not scoped_backends:
    return {}
  if scope.session_id is None:
    candidates = _opencode_candidate_ids(db, None)
    if candidates is None:
      return None
  else:
    candidates = []
    for backend_session in sorted(scoped_backends):
      backend_candidates = _opencode_candidate_ids(db, backend_session)
      if backend_candidates is None:
        return None
      candidates.extend(backend_candidates)
  if not candidates:
    return {}
  if scope.session_id is not None:
    safe_candidates = [aggregate_id for aggregate_id in candidates if scope.referenced_and_cold(aggregate_id)]
    return _opencode_aggregate_sizes(db, safe_candidates)
  referenced_cold: list[str] = []
  unreferenced: list[str] = []
  for aggregate_id in candidates:
    if scope.references.get(aggregate_id) is None:
      unreferenced.append(aggregate_id)
    elif scope.referenced_and_cold(aggregate_id):
      referenced_cold.append(aggregate_id)
  updated = _opencode_session_updated(db) if unreferenced else {}
  if updated is None:
    return None
  window_ids = [
      aggregate_id for aggregate_id in unreferenced
      if aggregate_id in updated and scope.idle_past(updated[aggregate_id] / 1000)
  ]
  return _opencode_aggregate_sizes(db, referenced_cold + window_ids)


def _sweep_opencode(db: Path, scope: usage_sources.SweepScope, counter: usage_sources.SweepCounter) -> None:
  """Delete cold/unreferenced aggregates' event rows in byte-capped chunks, then the sequence row.

  ``event`` carries the bytes: per aggregate, its rows go in delete transactions
  bounded by both ``_CHUNK_MAX_BYTES`` and ``_CHUNK_MAX_ROWS`` so a live writer
  interleaves instead of starving.  Once a read finds no event rows left, the
  ``event_sequence`` row goes exactly once and its ON DELETE CASCADE picks up any
  stragglers.  ``session``, ``message`` and ``part`` stay, and usage accounting
  keeps reading ``message``.
  """
  if not db.exists():
    return
  targets = _opencode_targets(db, scope)
  if targets is None:
    return
  if scope.dry_run:
    if not targets:
      return
    for _count, size in targets.values():
      counter.count += 1
      counter.bytes += size
    return
  if not targets:
    return
  try:
    connection = sqlite3.connect(db, timeout=SQLITE_LOCK_WAIT_SECONDS)
  except sqlite3.Error as e:
    log.warning("storage_cool_opencode_connect_failed", db=str(db), error=str(e))
    return
  try:
    # Cascade only fires with foreign keys on, and the pragma is per-connection.
    try:
      connection.execute("PRAGMA foreign_keys=ON")
    except sqlite3.Error as e:
      log.warning("storage_cool_opencode_setup_failed", db=str(db), error=str(e))
      return
    connection.isolation_level = None  # per-statement transactions: one failure keeps the rest
    for aggregate_id, (_count, size) in sorted(targets.items()):
      _delete_aggregate_events(connection, aggregate_id)
      try:
        cursor = connection.execute("DELETE FROM event_sequence WHERE aggregate_id = ?", (aggregate_id,))
      except sqlite3.Error as e:
        log.warning("storage_cool_opencode_delete_failed", aggregate_id=aggregate_id, error=str(e))
        continue
      if cursor.rowcount:
        counter.count += 1
        counter.bytes += size
  finally:
    connection.close()


def _delete_aggregate_events(connection: sqlite3.Connection, aggregate_id: str) -> None:
  """Delete one aggregate's ``event`` rows in capped transactions, one at a time.

  Each pass reads the next ``_CHUNK_MAX_ROWS`` rowids with their byte lengths
  (a read, so under WAL it takes no write lock) and deletes them client-side
  chunked by both the byte and the row cap, yielding between transactions so a
  live writer can interleave.  A failed chunk ends the loop for this aggregate:
  the sequence row's cascade and the next sweep pick up the stragglers, and the
  sweep never loops forever on a delete that keeps failing.
  """
  while True:
    rows = connection.execute(
        "SELECT rowid, length(data) FROM event WHERE aggregate_id = ? ORDER BY rowid LIMIT ?",
        (aggregate_id, _CHUNK_MAX_ROWS),
    ).fetchall()
    if not rows:
      return
    chunk: list[int] = []
    chunk_bytes = 0
    for rowid, data_length in rows:
      row_bytes = int(data_length)
      # A single row larger than the byte cap cannot be split, so it forms its own singleton chunk.
      if chunk and chunk_bytes + row_bytes > _CHUNK_MAX_BYTES:
        if not _delete_event_chunk(connection, aggregate_id, chunk):
          return
        time.sleep(_CHUNK_YIELD_SECONDS)
        chunk = []
        chunk_bytes = 0
      chunk.append(rowid)
      chunk_bytes += row_bytes
    if not _delete_event_chunk(connection, aggregate_id, chunk):
      return
    time.sleep(_CHUNK_YIELD_SECONDS)


def _delete_event_chunk(connection: sqlite3.Connection, aggregate_id: str, rowids: list[int]) -> bool:
  """One chunked delete transaction; False when the statement failed (logged, best effort)."""
  try:
    connection.execute(f"DELETE FROM event WHERE rowid IN ({','.join('?' * len(rowids))})", rowids)
  except sqlite3.Error as e:
    log.warning("storage_cool_opencode_delete_failed", aggregate_id=aggregate_id, rows=len(rowids), error=str(e))
    return False
  return True


def _vacuum_opencode_db(connection: sqlite3.Connection, db: Path, *, force: bool) -> None:
  """Hand the freed pages back to the filesystem; leave them for the next run on a lock loss."""
  try:
    connection.execute(f"PRAGMA busy_timeout={SQLITE_LOCK_WAIT_MS}")
    if not force:
      row = connection.execute("PRAGMA freelist_count").fetchone()
      if not row or not row[0]:
        return
    connection.execute("VACUUM")
  except sqlite3.Error as e:
    log.warning("storage_cool_opencode_vacuum_failed", db=str(db), error=str(e))


def _count_opencode_writers() -> int:
  """Live ``opencode serve`` processes on this host, excluding this process.

  CharlieBot spawns opencode servers with exactly that argv
  (src/backends/opencode/opencode.py builds ``[binary, "serve", ...]``), so a
  cmdline match is a live writer to the opencode store.
  """
  my_pid = os.getpid()
  count = 0
  for entry in Path("/proc").iterdir():
    if not entry.name.isdigit() or int(entry.name) == my_pid:
      continue
    try:
      cmdline = (entry / "cmdline").read_bytes()
    except OSError:
      continue  # the process vanished, or its cmdline is not readable by us
    if b"opencode serve" in cmdline.replace(b"\0", b" "):
      count += 1
  return count


def _vacuum_opencode_store(db: Path, *, force: bool) -> None:
  """Manually vacuum the opencode store, refusing past live writers unless *force*.

  Silently skipping a requested vacuum would be a silent fallback, so a refusal
  is a hard stop instead: an error line on stderr and exit code 1, before the
  database is touched for vacuum purposes at all.
  """
  writers = _count_opencode_writers()
  if writers and not force:
    print(
        f"Error: refusing to vacuum {db}: {writers} live opencode writer(s); "
        "stop them or re-run with --force.",
        file=sys.stderr,
    )
    raise SystemExit(1)
  if not db.exists():
    return
  try:
    connection = sqlite3.connect(db, timeout=SQLITE_LOCK_WAIT_SECONDS, isolation_level=None)
  except sqlite3.Error as e:
    log.warning("storage_cool_opencode_connect_failed", db=str(db), error=str(e))
    return
  try:
    _vacuum_opencode_db(connection, db, force=force)
  finally:
    connection.close()


def _opencode_freelist_bytes(db: Path) -> int:
  """Free pages the opencode store holds, in bytes; reclaimable by a manual vacuum."""
  if not db.exists():
    return 0
  pages = _opencode_query(db, "PRAGMA freelist_count")
  page_size = _opencode_query(db, "PRAGMA page_size")
  if not pages or not page_size:
    return 0
  return int(pages[0][0]) * int(page_size[0][0])


def sweep(scope: usage_sources.SweepScope) -> usage_sources.SourceSweep:
  """opencode's part of the sweep: the ``opencode-events`` category, counted in sessions, and the freelist.

  A requested vacuum runs after the deletes, and the freelist is read after it.
  """
  db = home.default_opencode_db()
  counter = usage_sources.SweepCounter("opencode-events", "sessions")
  _sweep_opencode(db, scope, counter)
  if scope.vacuum and not scope.dry_run:
    _vacuum_opencode_store(db, force=scope.force)
  return usage_sources.SourceSweep(
      categories=(counter.result(),),
      freelist=usage_sources.FreelistResult("opencode-freelist", _opencode_freelist_bytes(db)))
