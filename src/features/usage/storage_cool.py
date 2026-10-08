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
  minutes). Each backend deletes its own store: every registered usage source that
  defines ``sweep(scope)`` (src/runtime/hooks/usage_sources.py) runs in registration
  order and returns its categories. This module judges coldness and scans the metadata
  into the ``SweepScope`` that the sources read.

The sweep never compacts on its own: VACUUM of the opencode store is a manual
``--vacuum`` option that refuses while an ``opencode serve`` writer is alive
(``--force`` overrides) and reports the freelist either way.

Everything else a cold session holds — chat events, archives, thread events, fork
references, artifacts, uploads, HTML, metadata — is left byte-identical, so no read
path changes. Per session, per file, per statement the sweep is best effort: one
failure logs and the run continues. The one ordering the sweep enforces globally: a
real run first captures this host's token usage into the usage ledger and aborts
without deleting anything if that capture raises; a dry run never captures.
"""

import asyncio
import functools
import os
from dataclasses import dataclass
from datetime import UTC, datetime, timedelta
from pathlib import Path

from src.features.usage import token_tally, usage_ledger
from src.infra.config import CharlieBotConfig, get_config
from src.infra.constants import MIN_IDLE_DAYS
from src.infra.json_utils import load_json_meta
from src.infra.log_once import LazyStructlogLogger
from src.infra.models import SessionStatus, parse_utc_datetime
from src.runtime.hooks import usage_sources
from src.runtime.run_identity import SESSION_METADATA_NAME as METADATA_NAME
from src.runtime.runs import (
    CURSOR_NAME,
    DATA_DIR_NAME,
    MASTER_RUNS_DIR_NAME,
    RAW_LOG_NAME,
    RUN_METADATA_NAME,
    RUNS_DIR_NAME,
    STDERR_LOG_NAME,
    THREADS_DIR_NAME,
)

log = LazyStructlogLogger()

# Unreferenced backend records only have to outlive a backend session that is
# running right now; such a session refreshes its timestamp every few minutes.
ORPHAN_IDLE_DAYS = 2

# The five transport names a run's managed directories reserve, plus the numbered
# variants a re-spawn rotates them to (src/runtime/agent_process/base.py
# _rotate_stale_transport). Matching stays scoped to the managed directories.
# The three agent-pipe names come from runs.py, the module that owns the
# transport contract; the two tee names stay literals because their writers are
# backend modules (opencode.py, antigravity_cli.py) and core must not import
# backends.
RAW_TRANSPORT_NAMES = frozenset({
    RAW_LOG_NAME,
    STDERR_LOG_NAME,
    CURSOR_NAME,
    "stdout.log",
    "stderr.log",
})


@dataclass(frozen=True)
class SweepResult:
  """Per-category results of one sweep pass, plus the free pages of the databases that report them."""

  categories: tuple[usage_sources.CategoryResult, ...]
  freelists: tuple[usage_sources.FreelistResult, ...]

  @property
  def total_bytes(self) -> int:
    return sum(category.bytes for category in self.categories)

  def category(self, name: str) -> usage_sources.CategoryResult:
    for category in self.categories:
      if category.name == name:
        return category
    raise KeyError(name)


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


def _scan_sessions(cfg: CharlieBotConfig, now: datetime, min_idle_days: int) -> dict[str, usage_sources.SessionFacts]:
  """Read every session's metadata into the facts the sweep judges on."""
  facts: dict[str, usage_sources.SessionFacts] = {}
  sessions_dir = cfg.sessions_dir
  if not sessions_dir.is_dir():
    return facts
  for session_dir in sorted(sessions_dir.iterdir()):
    if not session_dir.is_dir():
      continue
    meta = load_json_meta(session_dir / METADATA_NAME, "storage_cool_meta_read_failed")
    if meta is None:
      continue
    facts[session_dir.name] = usage_sources.SessionFacts(
        id=session_dir.name,
        cold=is_cold_session(meta, now=now, min_idle_days=min_idle_days),
        cc_session_id=_optional_str(meta.get("cc_session_id")),
    )
  return facts


def _scan_references(cfg: CharlieBotConfig,
                     facts: dict[str, usage_sources.SessionFacts]) -> dict[str, list[usage_sources.SessionFacts]]:
  """Backend session ids CharlieBot metadata references, mapped to their referencing sessions.

  A session's ``cc_session_id`` is the reference its backend record hangs from;
  thread metadata is CharlieBot metadata too, so a thread's ``cc_session_id``
  (a legacy field the current model no longer persists) blocks reclamation
  exactly as a session's own does.
  """
  references: dict[str, list[usage_sources.SessionFacts]] = {}
  for owner in facts.values():
    if owner.cc_session_id is not None:
      references.setdefault(owner.cc_session_id, []).append(owner)
  for thread_dir in sorted(cfg.sessions_dir.glob(f"*/{THREADS_DIR_NAME}/*")):
    if not thread_dir.is_dir():
      continue
    meta = load_json_meta(thread_dir / METADATA_NAME, "storage_cool_thread_meta_read_failed")
    referenced_id = _optional_str((meta or {}).get("cc_session_id"))
    if referenced_id is None:
      continue
    owner = facts.get(thread_dir.parent.parent.name)
    if owner is None:
      log.warning("storage_cool_thread_reference_orphaned", thread=str(thread_dir), cc_session_id=referenced_id)
      # The thread metadata is still a CharlieBot reference even when its
      # parent session metadata has gone away.  Keep it conservative: an
      # unknown owner is not cold.
      owner = usage_sources.SessionFacts(id=thread_dir.parent.parent.name, cold=False, cc_session_id=None)
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
  data_root = session_dir / DATA_DIR_NAME
  master_runs = data_root / MASTER_RUNS_DIR_NAME
  if data_root.is_dir() and not data_root.is_symlink() and master_runs.is_dir() and not master_runs.is_symlink():
    run_dirs = usage_sources.sorted_scan(master_runs, master_runs.iterdir())
    if run_dirs is not None:
      managed.extend(child for child in run_dirs if child.is_dir() and not child.is_symlink())
  threads_dir = session_dir / THREADS_DIR_NAME
  if threads_dir.is_dir() and not threads_dir.is_symlink():
    thread_dirs = usage_sources.sorted_scan(threads_dir, threads_dir.iterdir()) or []
    for thread_dir in thread_dirs:
      if not thread_dir.is_dir() or thread_dir.is_symlink():
        continue
      data_dir = thread_dir / DATA_DIR_NAME
      if data_dir.is_dir() and not data_dir.is_symlink():
        managed.append(data_dir)
  return managed


def _run_referenced_transport(session_dir: Path) -> set[Path]:
  """Transport files a v2 Run record of this session references.

  Migrated task-tree runs point at their original evidence
  (``raw_log_ref``/``events_ref``/``result_ref``), so those files stay readable
  through the ordinary run read APIs after migration; the cold-session sweep
  must not reclaim them. Unreferenced transport files keep the existing
  contract.
  """
  runs_dir = session_dir / DATA_DIR_NAME / RUNS_DIR_NAME
  if not runs_dir.is_dir() or runs_dir.is_symlink():
    return set()
  referenced: set[str] = set()
  for run_dir in usage_sources.sorted_scan(runs_dir, runs_dir.iterdir()) or []:
    if not run_dir.is_dir() or run_dir.is_symlink():
      continue
    meta = load_json_meta(run_dir / RUN_METADATA_NAME, "storage_cool_run_meta_read_failed")
    if not isinstance(meta, dict):
      continue
    for key in ("raw_log_ref", "events_ref", "result_ref"):
      value = meta.get(key)
      if isinstance(value, str) and value:
        referenced.add(os.path.realpath(value))
  return referenced


def _sweep_raw_transport(session_dir: Path, counter: usage_sources.SweepCounter, dry_run: bool) -> None:
  """Delete the reserved transport names inside the session's managed run directories.

  Files a v2 Run record references are retention-protected: they are the
  imported run's own evidence and must stay resolvable (session-tree
  migration, plan 1 v4 section 4.2).
  """
  referenced = _run_referenced_transport(session_dir)
  for entry in usage_sources.sweep_root_entries(_managed_transport_dirs(session_dir), lambda root: root.iterdir()):
    if not entry.is_file() or not _is_transport_name(entry.name):
      continue
    if os.path.realpath(entry) in referenced:
      continue
    counter.delete_file(entry, dry_run)


# ---------------------------------------------------------------------------
# Entry point shared by the CLI and the scheduler handler
# ---------------------------------------------------------------------------


def _capture_usage_before_sweep() -> dict[str, int]:
  """Capture this host's token usage into the usage ledger; raises on failure so
  the sweep aborts before it can delete any source it is about to read."""
  with usage_ledger.UsageLedger(usage_ledger.default_ledger_path()) as ledger:
    return token_tally.capture_local(ledger)


def run_cool_sweep(
    *,
    dry_run: bool = False,
    min_idle_days: int = MIN_IDLE_DAYS,
    session_id: str | None = None,
    vacuum: bool = False,
    force: bool = False,
    cfg: CharlieBotConfig,
    now: datetime | None = None,
) -> SweepResult:
  """Run one storage sweep over cold sessions and unreferenced backend records.

  Args:
    dry_run: Report what would be freed without deleting anything, capturing
      usage, or issuing any SQL that changes the database; combines with
      *vacuum* by skipping it.
    min_idle_days: Idle age a session must reach, on top of being archived, to
      count as cold.
    session_id: Limit the whole sweep to one session; the cold rule still applies,
      so a session that is not cold leaves the sweep nothing to do.
    vacuum: After the sweep, VACUUM the opencode store to hand freed pages back to
      the filesystem. Refuses (stderr + SystemExit) while an ``opencode serve``
      writer is alive, unless *force*.
    force: Vacuum past live-writer refusal; no effect without *vacuum*.
    cfg: Config to read paths and backend options from.
    now: Current time override for tests.

  Returns:
    Per-category counts and freed bytes in report order, plus the free pages of each database
    that a source reports.
  """
  now = now or datetime.now(UTC)
  if not dry_run:
    # The ledger capture guards every deletion: it runs before the scan and any
    # sweep step, and a failure propagates so the round reports an error and
    # this sweep deletes nothing.
    _capture_usage_before_sweep()
  facts = _scan_sessions(cfg, now, min_idle_days)
  references = _scan_references(cfg, facts)
  if session_id is not None:
    if session_id not in facts:
      raise ValueError(f"session not found: {session_id}")
    owner = facts[session_id]
    # The cold rule holds in a scoped run too: a session that is not cold leaves
    # every category nothing to do.
    facts = {session_id: owner} if owner.cold else {}

  transport = usage_sources.SweepCounter("raw-transport", "files")
  for cold_id, cold_facts in facts.items():
    if cold_facts.cold:
      _sweep_raw_transport(cfg.sessions_dir / cold_id, transport, dry_run)
  scope = usage_sources.SweepScope(
      cfg=cfg,
      now=now,
      dry_run=dry_run,
      session_id=session_id,
      facts=facts,
      references=references,
      orphan_idle_days=ORPHAN_IDLE_DAYS,
      vacuum=vacuum,
      force=force)
  source_sweeps = usage_sources.sweep_all(scope)

  result = SweepResult(
      categories=(transport.result(), *(category for swept in source_sweeps for category in swept.categories)),
      freelists=tuple(swept.freelist for swept in source_sweeps if swept.freelist is not None))
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


async def run_scheduled_cool_storage() -> str:
  """Built-in cron handler: reclaim cold sessions' readerless bytes (real run, no dry run)."""
  # storage_cool (token_tally, the ledger) rides the handler like croniter: the
  # registration holds this module's path and the scheduler imports it when the
  # handler fires, so the M99 server import floor carries no cold-sweep stack for
  # a handler that may never fire.
  loop = asyncio.get_running_loop()
  result = await loop.run_in_executor(None, functools.partial(run_cool_sweep, cfg=get_config()))
  summary = format_sweep_line(result)
  log.info('cool_storage_handler_done', total_bytes=result.total_bytes)
  return summary


def format_sweep_table(result: SweepResult) -> str:
  """The command's report: one line per category, then one per freelist, then the total."""
  lines = [
      f"{category.name:<18}{category.count:>7} {category.unit:<9}{_gib(category.bytes):>9.2f} GiB"
      for category in result.categories
  ]
  lines.extend(
      f"{freelist.name:<18}{'':>17}{_gib(freelist.bytes):>9.2f} GiB (reclaimable via --vacuum)"
      for freelist in result.freelists)
  lines.append(f"{'total':<18}{'':>17}{_gib(result.total_bytes):>9.2f} GiB")
  return "\n".join(lines)


def _gib(num_bytes: int) -> float:
  return num_bytes / (1024**3)
