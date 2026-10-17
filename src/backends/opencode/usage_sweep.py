"""opencode's part of the cold-storage sweep (src/features/usage/storage_cool.py): no deletion, the manual vacuum.

``usage_logs.sweep()`` imports this module on its first call, so a usage capture and
``charliebot --help`` never load it. The sweep deletes no row of the 2.0 store in any mode: its ``event`` table
stays empty in use, and ``event_sequence`` is the sequence table live writes use. The usage log reads
``session_message``, which the sweep never touches.

The sweep never compacts on its own: VACUUM is the manual ``--vacuum`` option, which refuses while an
``opencode serve`` writer is alive (``--force`` overrides). The report carries the freelist either way.
"""

import os
import pathlib
import sqlite3
import sys

from src.infra import home, log_once, timeouts
from src.runtime.hooks import usage_sources

log = log_once.LazyStructlogLogger()


def _opencode_query(db: pathlib.Path, sql: str) -> list[tuple] | None:
  """One read-only query against the opencode store; None on failure, [] on an empty result."""
  try:
    connection = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    try:
      return connection.execute(sql).fetchall()
    finally:
      connection.close()
  except sqlite3.Error as e:
    log.warning("storage_cool_opencode_scan_failed", db=str(db), error=str(e))
    return None


def _vacuum_opencode_db(connection: sqlite3.Connection, db: pathlib.Path, *, force: bool) -> None:
  """Hand the freed pages back to the filesystem; leave them for the next run on a lock loss."""
  try:
    connection.execute(f"PRAGMA busy_timeout={timeouts.SQLITE_LOCK_WAIT_MS}")
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
  for entry in pathlib.Path("/proc").iterdir():
    if not entry.name.isdigit() or int(entry.name) == my_pid:
      continue
    try:
      cmdline = (entry / "cmdline").read_bytes()
    except OSError:
      continue  # the process vanished, or its cmdline is not readable by us
    if b"opencode serve" in cmdline.replace(b"\0", b" "):
      count += 1
  return count


def _vacuum_opencode_store(db: pathlib.Path, *, force: bool) -> None:
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
    connection = sqlite3.connect(db, timeout=timeouts.SQLITE_LOCK_WAIT_SECONDS, isolation_level=None)
  except sqlite3.Error as e:
    log.warning("storage_cool_opencode_connect_failed", db=str(db), error=str(e))
    return
  try:
    _vacuum_opencode_db(connection, db, force=force)
  finally:
    connection.close()


def _opencode_freelist_bytes(db: pathlib.Path) -> int:
  """Free pages the opencode store holds, in bytes; reclaimable by a manual vacuum."""
  if not db.exists():
    return 0
  pages = _opencode_query(db, "PRAGMA freelist_count")
  page_size = _opencode_query(db, "PRAGMA page_size")
  if not pages or not page_size:
    return 0
  return int(pages[0][0]) * int(page_size[0][0])


def sweep(scope: usage_sources.SweepScope) -> usage_sources.SourceSweep:
  """opencode's part of the sweep: no category, a requested vacuum, and the freelist.

  The store is never deleted from, so the sweep reports no category. A requested vacuum runs
  before the freelist is read, so the freelist reflects it.
  """
  db = home.opencode_db()
  if scope.vacuum and not scope.dry_run:
    _vacuum_opencode_store(db, force=scope.force)
  return usage_sources.SourceSweep(
      categories=(), freelist=usage_sources.FreelistResult("opencode-freelist", _opencode_freelist_bytes(db)))
