"""Cold-session storage sweep: delete conversation bytes that no reader can reach again.

A session goes cold when its metadata says ``archived`` and its ``updated_at`` is at
least ``MIN_IDLE_DAYS`` old; the judgment is recomputed from those existing fields on
every scan, so the sweep persists no marker of its own and rerunning it is a no-op.
The sweep reclaims exactly two things the approved design (plan 1 v10) names:

- Raw transport files of cold sessions, scoped by relative path: the five reserved
  names (plus the ``<name>.<N>`` rotation variants ``_rotate_stale_transport`` leaves)
  inside ``data/master_runs/<started_at>/`` and ``threads/<thread_id>/data/`` only.
  ``uploads/stdout.log`` is a legitimate user file — the upload endpoint keeps the
  client's filename verbatim — so the rule is an allowlist of names inside an
  allowlist of directories, never a name-only match.

- Backend conversation stores under one rule: a record goes when the session
  referencing it is cold, or when no CharlieBot metadata references it and it has
  been idle past ``ORPHAN_IDLE_DAYS`` (the window only has to cover a backend
  session that is running right now, which refreshes its timestamp every few
  minutes). Claude Code transcript directories are recognized by the cwd encoding
  the CLI applies (separators, dots and underscores become hyphens); Codex rollout
  file names embed the thread id; the opencode store loses ``event_sequence`` rows,
  whose ``event`` rows disappear by foreign-key cascade.

Everything else a cold session holds — chat events, archives, thread events, fork
references, artifacts, uploads, HTML, metadata — is left byte-identical, so no read
path changes. Per session, per file, per statement the sweep is best effort: one
failure logs and the run continues.
"""

import os
import re
import shutil
import sqlite3
from dataclasses import dataclass
from datetime import UTC, datetime, timedelta
from pathlib import Path

import structlog

from src.core.config import (
    BackendOption,
    CharlieBotConfig,
    claude_config_dir,
    get_config,
)
from src.core.json_utils import load_json_meta
from src.core.models import SessionStatus, parse_utc_datetime
from src.core.timeouts import SQLITE_LOCK_WAIT_MS, SQLITE_LOCK_WAIT_SECONDS
from src.core.token_tally import DEFAULT_OPENCODE_DB

log = structlog.get_logger()

MIN_IDLE_DAYS = 14
# Unreferenced backend records only have to outlive a backend session that is
# running right now; such a session refreshes its timestamp every few minutes.
ORPHAN_IDLE_DAYS = 2

# The five transport names a run's managed directories reserve, plus the numbered
# variants a re-spawn rotates them to (src/agents/backends/base.py
# _rotate_stale_transport). Matching stays scoped to the managed directories.
RAW_TRANSPORT_NAMES = frozenset(
    {
        "agent.raw.ndjson",
        "agent.stderr.log",
        "agent.raw.cursor",
        "stdout.log",
        "stderr.log",
    })

# Canonical UUID form of a CharlieBot session id, as it appears verbatim inside a
# Claude Code transcript directory name (hyphens pass the cwd encoding through).
_SESSION_ID_RE = re.compile(r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}")
# Claude Code derives the transcript directory from the process cwd; every cwd
# CharlieBot hands a claude process is the session directory itself or a
# directory inside it, so the encoded name carries the session id after the
# encoded "sessions" path segment.
_SESSIONS_SEGMENT = "-sessions-"

_CODEX_ROLLOUT_PREFIX = "rollout-"

WORKTREE_TRASH_DIR = ".trash"

_SQL_PARAM_CHUNK = 500


@dataclass(frozen=True)
class CategoryResult:
  """One output category: what the sweep would free (or freed)."""

  name: str
  unit: str
  count: int
  bytes: int


@dataclass(frozen=True)
class SweepResult:
  """Per-category results of one sweep pass."""

  categories: tuple[CategoryResult, ...]

  @property
  def total_bytes(self) -> int:
    return sum(category.bytes for category in self.categories)

  def category(self, name: str) -> CategoryResult:
    for category in self.categories:
      if category.name == name:
        return category
    raise KeyError(name)


@dataclass(frozen=True)
class _SessionFacts:
  """The metadata fields the sweep's judgments read."""

  id: str
  cold: bool
  cc_session_id: str | None


class _Counter:
  """Accumulates one category's count and freed bytes."""

  def __init__(self, name: str, unit: str) -> None:
    self.name = name
    self.unit = unit
    self.count = 0
    self.bytes = 0

  def add(self, size: int) -> None:
    self.count += 1
    self.bytes += size

  def add_bytes(self, size: int) -> None:
    """Bytes without a count: the unit belongs to a larger whole (a directory)."""
    self.bytes += size


# ---------------------------------------------------------------------------
# Cold rule and metadata scan
# ---------------------------------------------------------------------------


def is_cold_session(meta: dict, *, now: datetime, min_idle_days: int) -> bool:
  """The one cold rule: archived, and idle at least *min_idle_days*.

  Reads only fields session metadata already persists. A session whose metadata is
  missing or unreadable never qualifies.
  """
  if str(meta.get("status")) != SessionStatus.ARCHIVED.value:
    return False
  raw_updated = meta.get("updated_at")
  if not raw_updated:
    return False
  try:
    updated_at = parse_utc_datetime(str(raw_updated))
  except ValueError:
    return False
  return now - updated_at >= timedelta(days=min_idle_days)


def _scan_sessions(cfg: CharlieBotConfig, now: datetime, min_idle_days: int) -> dict[str, _SessionFacts]:
  """Read every session's metadata into the facts the sweep judges on."""
  facts: dict[str, _SessionFacts] = {}
  sessions_dir = cfg.sessions_dir
  if not sessions_dir.is_dir():
    return facts
  for session_dir in sorted(sessions_dir.iterdir()):
    if not session_dir.is_dir():
      continue
    meta = load_json_meta(session_dir / "metadata.json", "storage_cool_meta_read_failed")
    if meta is None:
      continue
    facts[session_dir.name] = _SessionFacts(
        id=session_dir.name,
        cold=is_cold_session(meta, now=now, min_idle_days=min_idle_days),
        cc_session_id=_optional_str(meta.get("cc_session_id")),
    )
  return facts


def _scan_references(cfg: CharlieBotConfig, facts: dict[str, _SessionFacts]) -> dict[str, list[_SessionFacts]]:
  """Backend session ids CharlieBot metadata references, mapped to their referencing sessions.

  A session's ``cc_session_id`` is the reference its backend record hangs from;
  thread metadata is CharlieBot metadata too, so a thread's ``cc_session_id``
  (a legacy field the current model no longer persists) blocks reclamation
  exactly as a session's own does.
  """
  references: dict[str, list[_SessionFacts]] = {}
  for owner in facts.values():
    if owner.cc_session_id is not None:
      references.setdefault(owner.cc_session_id, []).append(owner)
  for thread_dir in sorted(cfg.sessions_dir.glob("*/threads/*")):
    if not thread_dir.is_dir():
      continue
    meta = load_json_meta(thread_dir / "metadata.json", "storage_cool_thread_meta_read_failed")
    referenced_id = _optional_str((meta or {}).get("cc_session_id"))
    if referenced_id is None:
      continue
    owner = facts.get(thread_dir.parent.parent.name)
    if owner is None:
      log.warning("storage_cool_thread_reference_orphaned", thread=str(thread_dir), cc_session_id=referenced_id)
      # The thread metadata is still a CharlieBot reference even when its
      # parent session metadata has gone away.  Keep it conservative: an
      # unknown owner is not cold.
      owner = _SessionFacts(id=thread_dir.parent.parent.name, cold=False, cc_session_id=None)
    references.setdefault(referenced_id, []).append(owner)
  return references


def _optional_str(value: object) -> str | None:
  return str(value) if value else None


# ---------------------------------------------------------------------------
# Part 1: raw transport files of cold sessions
# ---------------------------------------------------------------------------


def _is_transport_name(name: str) -> bool:
  """One of the five reserved transport names, or its numbered rotation variant."""
  if name in RAW_TRANSPORT_NAMES:
    return True
  base, separator, suffix = name.rpartition(".")
  return bool(separator) and base in RAW_TRANSPORT_NAMES and suffix.isdigit()


def _managed_transport_dirs(session_dir: Path) -> list[Path]:
  """The two directory shapes whose direct children the transport rule governs."""
  managed: list[Path] = []
  data_root = session_dir / "data"
  master_runs = data_root / "master_runs"
  if data_root.is_dir() and not data_root.is_symlink() and master_runs.is_dir() and not master_runs.is_symlink():
    try:
      run_dirs = sorted(master_runs.iterdir())
    except OSError as e:
      log.warning("storage_cool_dir_scan_failed", dir=str(master_runs), error=str(e))
    else:
      managed.extend(child for child in run_dirs if child.is_dir() and not child.is_symlink())
  threads_dir = session_dir / "threads"
  if threads_dir.is_dir() and not threads_dir.is_symlink():
    try:
      thread_dirs = sorted(threads_dir.iterdir())
    except OSError as e:
      log.warning("storage_cool_dir_scan_failed", dir=str(threads_dir), error=str(e))
      thread_dirs = []
    for thread_dir in thread_dirs:
      if not thread_dir.is_dir() or thread_dir.is_symlink():
        continue
      data_dir = thread_dir / "data"
      if data_dir.is_dir() and not data_dir.is_symlink():
        managed.append(data_dir)
  return managed


def _sweep_raw_transport(session_dir: Path, counter: _Counter, dry_run: bool) -> None:
  """Delete the reserved transport names inside the session's managed run directories."""
  for managed_dir in _managed_transport_dirs(session_dir):
    try:
      entries = sorted(managed_dir.iterdir())
    except OSError as e:
      log.warning("storage_cool_dir_scan_failed", dir=str(managed_dir), error=str(e))
      continue
    for entry in entries:
      if not entry.is_file() or not _is_transport_name(entry.name):
        continue
      _delete_file(entry, counter, dry_run)


def _delete_file(path: Path, counter: _Counter, dry_run: bool, *, count: bool = True) -> None:
  """Delete one file (or account for it in a dry run), best effort per file.

  ``count=False`` folds the bytes into a category whose unit is larger than a
  file (a transcript directory).
  """
  try:
    size = path.stat().st_size
  except OSError as e:
    log.warning("storage_cool_file_stat_failed", path=str(path), error=str(e))
    return
  if dry_run:
    if count:
      counter.add(size)
    else:
      counter.add_bytes(size)
    return
  try:
    path.unlink()
  except OSError as e:
    log.warning("storage_cool_file_delete_failed", path=str(path), error=str(e))
    return
  if count:
    counter.add(size)
  else:
    counter.add_bytes(size)


# ---------------------------------------------------------------------------
# Part 2a: Claude Code transcript directories
# ---------------------------------------------------------------------------


def claude_project_dir_name(cwd: Path) -> str:
  """The transcript directory name Claude Code derives from a process cwd.

  Path separators, dots and underscores each become a hyphen; every other
  character passes through.
  """
  return str(cwd).replace("/", "-").replace(".", "-").replace("_", "-")


def claude_projects_roots(cfg: CharlieBotConfig) -> list[Path]:
  """Every ``projects`` tree a cc-claude backend may have written transcripts into.

  A transcript tree outlives the environment that created it, so the search set
  must not depend on the current one: the default home is seeded beside the
  ``claude_config_dir`` answers for the environment and the configured cc-claude
  options, mirroring how ``codex_session_trees`` seeds its store.  Widening the
  searched trees does not widen what is deletable: a name that encodes neither a
  CharlieBot session id nor the worktree prefix stays untouched no matter which
  tree it sits in.
  """
  homes = {Path.home() / ".claude", claude_config_dir(BackendOption(id="-", label="-", type="cc-claude"))}
  for option in cfg.backend_options:
    if option.type == "cc-claude":
      homes.add(claude_config_dir(option))
  return sorted(home / "projects" for home in homes)


def _encoded_session_id(dir_name: str) -> str | None:
  """The CharlieBot session id a transcript directory name encodes, or None.

  The encoded cwd of anything inside a session directory reads
  ``<prefix>-sessions-<session id>[-<more>]``, so the name is CharlieBot's own
  exactly when the tail after the last ``-sessions-`` starts with a full session
  id and either ends there or continues with another path segment.
  """
  start = dir_name.rfind(_SESSIONS_SEGMENT)
  if start < 0:
    return None
  tail = dir_name[start + len(_SESSIONS_SEGMENT):]
  match = _SESSION_ID_RE.match(tail)
  if match is None:
    return None
  rest = tail[match.end():]
  if rest and not rest.startswith("-"):
    return None
  return match.group(0)


def _newest_mtime(path: Path) -> float | None:
  """Newest file mtime under *path*, or the directory's own when it holds none."""
  newest: float | None = None
  for dirpath, _, filenames in os.walk(path):
    for filename in filenames:
      try:
        mtime = os.stat(os.path.join(dirpath, filename)).st_mtime
      except OSError:
        continue
      newest = mtime if newest is None else max(newest, mtime)
  if newest is None:
    try:
      newest = os.stat(path).st_mtime
    except OSError:
      return None
  return newest


def _idle_past_window(path: Path, now: datetime, idle_days: int) -> bool:
  newest = _newest_mtime(path)
  if newest is None:
    log.warning("storage_cool_idle_probe_failed", path=str(path))
    return False
  return now.timestamp() - newest >= idle_days * 86400


def _live_worktree_dir_names(cfg: CharlieBotConfig) -> set[str]:
  """Encoded cwd names of the worktrees currently on disk (their runs may still write)."""
  worktree_dir = Path(cfg.worktree_dir)
  if not worktree_dir.is_dir():
    return set()
  return {
      claude_project_dir_name(child)
      for child in worktree_dir.iterdir()
      if child.is_dir() and child.name != WORKTREE_TRASH_DIR
  }


def _delete_claude_project_dir(path: Path, counter: _Counter, dry_run: bool) -> None:
  """Delete a transcript directory file by file, then its empty skeleton."""
  for file_path in sorted(path.rglob("*")):
    if file_path.is_file():
      _delete_file(file_path, counter, dry_run, count=False)
  if dry_run:
    counter.count += 1
    return
  try:
    shutil.rmtree(path)
  except OSError as e:
    log.warning("storage_cool_dir_delete_failed", path=str(path), error=str(e))
    return
  counter.count += 1


def _sweep_claude_transcripts(
    cfg: CharlieBotConfig,
    facts: dict[str, _SessionFacts],
    now: datetime,
    counter: _Counter,
    *,
    dry_run: bool,
    session_id: str | None,
) -> None:
  """Delete cold sessions' transcript directories and unreferenced orphan directories.

  A transcript directory belongs to CharlieBot when its encoded name carries a
  session id. That session being cold is the forward-computed deletion case; the
  session having no metadata left makes the directory an orphan, reclaimable once
  it has been idle past the safety window. Worktree-encoded directories follow the
  same orphan rule once the worktree itself is gone. Any other name encodes a cwd
  CharlieBot never handed a claude process and is never touched.
  """
  worktree_prefix = claude_project_dir_name(Path(cfg.worktree_dir)) + "-"
  live_worktrees = _live_worktree_dir_names(cfg) if session_id is None else set()
  for projects_root in claude_projects_roots(cfg):
    if not projects_root.is_dir():
      continue
    try:
      entries = sorted(projects_root.iterdir())
    except OSError as e:
      log.warning("storage_cool_dir_scan_failed", dir=str(projects_root), error=str(e))
      continue
    for entry in entries:
      if not entry.is_dir():
        continue
      encoded_session = _encoded_session_id(entry.name)
      if encoded_session is not None:
        owner = facts.get(encoded_session)
        if owner is not None:
          # The session still exists: only the cold rule decides.
          if owner.cold:
            _delete_claude_project_dir(entry, counter, dry_run)
        elif (cfg.sessions_dir / encoded_session).is_dir():
          # An unreadable session metadata file is not proof that the session
          # was deleted; only a missing session directory makes this an orphan.
          continue
        elif session_id is None and _idle_past_window(entry, now, ORPHAN_IDLE_DAYS):
          # No metadata references it any more: orphan past the window.
          _delete_claude_project_dir(entry, counter, dry_run)
      elif session_id is None and entry.name.startswith(worktree_prefix) and entry.name not in live_worktrees:
        # A worktree's transcripts are orphans once the worktree is gone.
        if _idle_past_window(entry, now, ORPHAN_IDLE_DAYS):
          _delete_claude_project_dir(entry, counter, dry_run)


# ---------------------------------------------------------------------------
# Part 2b: Codex rollout files
# ---------------------------------------------------------------------------


def codex_session_trees(cfg: CharlieBotConfig) -> list[Path]:
  """Every rollout tree a configured codex backend writes into, default included."""
  homes = {Path.home() / ".codex"}
  for option in cfg.backend_options:
    if option.type == "codex" and option.codex_home:
      homes.add(Path(option.codex_home).expanduser())
  return sorted(home / "sessions" for home in homes)


def codex_rollout_session_id(path: Path) -> str | None:
  """The codex thread id embedded in ``rollout-<iso timestamp>-<uuid>.jsonl``.

  The timestamp itself contains hyphens, so the id is the last five
  hyphen-separated components of the stem, not the first five.
  """
  stem = path.stem
  if not stem.startswith(_CODEX_ROLLOUT_PREFIX):
    return None
  candidate = "-".join(stem.split("-")[-5:])
  return candidate if _SESSION_ID_RE.fullmatch(candidate) else None


def _sweep_codex_rollouts(
    cfg: CharlieBotConfig,
    facts: dict[str, _SessionFacts],
    references: dict[str, list[_SessionFacts]],
    now: datetime,
    counter: _Counter,
    *,
    dry_run: bool,
    session_id: str | None,
) -> None:
  """Delete rollout files under the session-cold / unreferenced-plus-window rule."""
  scoped_backends = _scoped_backend_sessions(facts, references, session_id) if session_id is not None else set()
  for tree in codex_session_trees(cfg):
    if not tree.is_dir():
      continue
    try:
      candidates = sorted(tree.rglob(f"{_CODEX_ROLLOUT_PREFIX}*.jsonl"))
    except OSError as e:
      log.warning("storage_cool_dir_scan_failed", dir=str(tree), error=str(e))
      continue
    for path in candidates:
      backend_session = codex_rollout_session_id(path)
      if backend_session is None:
        continue
      if session_id is not None:
        # Scoped run: only the named session's own record, already cold-verified,
        # and never a record also referenced by a live session.
        referencing = references.get(backend_session)
        if backend_session in scoped_backends and referencing and all(owner.cold for owner in referencing):
          _delete_file(path, counter, dry_run)
        continue
      referencing = references.get(backend_session)
      if referencing:
        if all(owner.cold for owner in referencing):
          _delete_file(path, counter, dry_run)
        continue
      try:
        idle_since = datetime.fromtimestamp(path.stat().st_mtime, tz=UTC)
      except OSError as e:
        log.warning("storage_cool_file_stat_failed", path=str(path), error=str(e))
        continue
      if now - idle_since >= timedelta(days=ORPHAN_IDLE_DAYS):
        _delete_file(path, counter, dry_run)


def _scoped_backend_sessions(
    facts: dict[str, _SessionFacts],
    references: dict[str, list[_SessionFacts]],
    session_id: str,
) -> set[str]:
  """Return every backend id referenced by the one scoped session."""
  owner = facts.get(session_id)
  backend_sessions = {owner.cc_session_id} if owner and owner.cc_session_id is not None else set()
  backend_sessions.update(
      backend_session for backend_session, owners in references.items() if any(
          reference.id == session_id for reference in owners))
  return backend_sessions


# ---------------------------------------------------------------------------
# Part 2c: opencode event store
# ---------------------------------------------------------------------------

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
  sizes: dict[str, tuple[int, int]] = {aggregate_id: (0, 0) for aggregate_id in aggregate_ids}
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


def _opencode_targets(
    db: Path,
    facts: dict[str, _SessionFacts],
    references: dict[str, list[_SessionFacts]],
    now: datetime,
    session_id: str | None,
) -> dict[str, tuple[int, int]] | None:
  """Aggregate ids the rule deletes, with their event counts and byte totals.

  In a scoped run the named session's own aggregate is the only candidate (the
  cold rule is already established for it); otherwise a referenced aggregate goes
  when every referencing session is cold, and an unreferenced one once the
  backend's own timestamp has been idle past the safety window.
  """
  scoped_backends = _scoped_backend_sessions(facts, references, session_id) if session_id is not None else set()
  if session_id is not None and not scoped_backends:
    return {}
  if session_id is None:
    candidates = _opencode_candidate_ids(db, None)
  else:
    candidates = []
    for backend_session in sorted(scoped_backends):
      backend_candidates = _opencode_candidate_ids(db, backend_session)
      if backend_candidates is None:
        return None
      candidates.extend(backend_candidates)
  if candidates is None:
    return None
  if not candidates:
    return {}
  if session_id is not None:
    safe_candidates = [
        aggregate_id for aggregate_id in candidates
        if (referencing := references.get(aggregate_id)) and all(owner.cold for owner in referencing)
    ]
    return _opencode_aggregate_sizes(db, safe_candidates)
  referenced_cold: list[str] = []
  unreferenced: list[str] = []
  for aggregate_id in candidates:
    referencing = references.get(aggregate_id)
    if referencing is None:
      unreferenced.append(aggregate_id)
    elif all(owner.cold for owner in referencing):
      referenced_cold.append(aggregate_id)
  updated = _opencode_session_updated(db) if unreferenced else {}
  if updated is None:
    return None
  window_ids = [
      aggregate_id for aggregate_id in unreferenced
      if aggregate_id in updated and now.timestamp() - updated[aggregate_id] / 1000 >= ORPHAN_IDLE_DAYS * 86400
  ]
  return _opencode_aggregate_sizes(db, referenced_cold + window_ids)


def _sweep_opencode(
    db: Path,
    facts: dict[str, _SessionFacts],
    references: dict[str, list[_SessionFacts]],
    now: datetime,
    counter: _Counter,
    *,
    dry_run: bool,
    session_id: str | None,
) -> None:
  """Delete cold/unreferenced aggregates' event rows through the sequence table.

  ``event`` carries the bytes and hangs off ``event_sequence`` by an
  ON DELETE CASCADE foreign key, so deleting the sequence row per aggregate is
  the whole deletion; ``session``, ``message`` and ``part`` stay, and usage
  accounting keeps reading ``message``.
  """
  if not db.exists():
    return
  targets = _opencode_targets(db, facts, references, now, session_id)
  if targets is None:
    return
  if dry_run:
    if not targets:
      return
    for count, size in targets.values():
      counter.count += 1
      counter.bytes += size
    return
  # A scoped run must not compact unrelated database pages when its session is
  # active or has no safely deletable backend record.  The unscoped scheduler
  # pass is responsible for retrying a prior VACUUM lock failure.
  if session_id is not None and not targets:
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
    deleted = False
    for aggregate_id, (count, size) in sorted(targets.items()):
      try:
        cursor = connection.execute("DELETE FROM event_sequence WHERE aggregate_id = ?", (aggregate_id,))
      except sqlite3.Error as e:
        log.warning("storage_cool_opencode_delete_failed", aggregate_id=aggregate_id, error=str(e))
        continue
      if cursor.rowcount:
        deleted = True
        counter.count += 1
        counter.bytes += size
    # A failed VACUUM leaves freelist pages behind.  Keep opening the database
    # on later no-target runs so the short-lock attempt can be retried.
    _vacuum_opencode_db(connection, db, force=deleted)
  finally:
    connection.close()


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


# ---------------------------------------------------------------------------
# Entry point shared by the CLI and the scheduler handler
# ---------------------------------------------------------------------------


def run_cool_sweep(
    *,
    dry_run: bool = False,
    min_idle_days: int = MIN_IDLE_DAYS,
    session_id: str | None = None,
    cfg: CharlieBotConfig | None = None,
    now: datetime | None = None,
) -> SweepResult:
  """Run one storage sweep over cold sessions and unreferenced backend records.

  Args:
    dry_run: Report what would be freed without deleting anything or issuing any
      SQL that changes the database.
    min_idle_days: Idle age a session must reach, on top of being archived, to
      count as cold.
    session_id: Limit the whole sweep to one session; the cold rule still applies,
      so a session that is not cold leaves the sweep nothing to do.
    cfg: Config to read paths and backend options from; defaults to the process
      config.
    now: Current time override for tests.

  Returns:
    Per-category counts and freed bytes.
  """
  cfg = cfg or get_config()
  now = now or datetime.now(UTC)
  facts = _scan_sessions(cfg, now, min_idle_days)
  references = _scan_references(cfg, facts)
  if session_id is not None:
    if session_id not in facts:
      raise ValueError(f"session not found: {session_id}")
    owner = facts[session_id]
    # The cold rule holds in a scoped run too: a session that is not cold leaves
    # every category nothing to do.
    facts = {session_id: owner} if owner.cold else {}

  transport = _Counter("raw-transport", "files")
  claude = _Counter("claude-transcripts", "dirs")
  codex = _Counter("codex-rollouts", "files")
  opencode = _Counter("opencode-events", "sessions")
  for cold_id, cold_facts in facts.items():
    if cold_facts.cold:
      _sweep_raw_transport(cfg.sessions_dir / cold_id, transport, dry_run)
  _sweep_claude_transcripts(cfg, facts, now, claude, dry_run=dry_run, session_id=session_id)
  _sweep_codex_rollouts(cfg, facts, references, now, codex, dry_run=dry_run, session_id=session_id)
  _sweep_opencode(DEFAULT_OPENCODE_DB, facts, references, now, opencode, dry_run=dry_run, session_id=session_id)

  result = SweepResult(
      categories=tuple(
          [
              CategoryResult(transport.name, transport.unit, transport.count, transport.bytes),
              CategoryResult(claude.name, claude.unit, claude.count, claude.bytes),
              CategoryResult(codex.name, codex.unit, codex.count, codex.bytes),
              CategoryResult(opencode.name, opencode.unit, opencode.count, opencode.bytes),
          ]))
  log.info(
      "storage_cool_sweep_done",
      dry_run=dry_run,
      session_id=session_id,
      total_bytes=result.total_bytes,
      **{category.name.replace("-", "_"): category.count for category in result.categories},
  )
  return result


def format_sweep_line(result: SweepResult) -> str:
  """One-line summary for the scheduler handler's result message."""
  parts = [
      f"{category.name} {category.count} {category.unit} {_gib(category.bytes):.2f} GiB"
      for category in result.categories
  ]
  return f"total {_gib(result.total_bytes):.2f} GiB ({'; '.join(parts)})"


def format_sweep_table(result: SweepResult) -> str:
  """The command's report: one line per category, then the total."""
  lines = [
      f"{category.name:<18}{category.count:>7} {category.unit:<9}{_gib(category.bytes):>9.2f} GiB"
      for category in result.categories
  ]
  lines.append(f"{'total':<18}{'':>17}{_gib(result.total_bytes):>9.2f} GiB")
  return "\n".join(lines)


def _gib(num_bytes: int) -> float:
  return num_bytes / (1024**3)
