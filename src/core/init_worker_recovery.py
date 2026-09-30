"""Thread metadata scans for the read-only views plus the startup worktree quarantine."""

from __future__ import annotations

import asyncio
import os
import stat
from collections.abc import Callable, Iterator
from datetime import datetime, timedelta
from pathlib import Path
from typing import TYPE_CHECKING

if TYPE_CHECKING:
  from src.core.config import CharlieBotConfig
  from src.core.sessions import SessionManager

from src.core import event_types as ET
from src.core import runs
from src.core.git import git_quarantine_worktree, git_worktree_dir_name
from src.core.human_size import format_size
from src.core.json_utils import load_json_meta
from src.core.log_once import LazyStructlogLogger
from src.core.memo import StatSignatureMemo
from src.core.models import (
    TERMINAL_THREAD_STATUSES,
    parse_utc_datetime,
    utc_now,
)
from src.core.threads import METADATA_NAME, THREADS_DIR_NAME
from src.core.worktree_trash import dir_size_bytes, trash_dir

log = LazyStructlogLogger()

# Failed worktrees older than this are swept into <worktree_dir>/.trash/ on startup.
# Kept (not hard-deleted) so recent failures stay available for debugging.
FAILED_WORKTREE_QUARANTINE_DAYS = 7

# Only threads whose metadata.json was modified within this window are read+parsed
# by the orphan-recovery scan and the live "has running task" badge. A running
# thread's metadata.json is written when it starts and is NOT rewritten while it
# runs, so a live thread is always recent; reading every thread's metadata cold to
# find status=="running" is a full-history Lustre scan (~18s over ~1174 files on a
# network FS) where a scandir+stat-then-read is ~15x cheaper. This is semantically
# correct, not a heuristic: a metadata that is old yet still says "running" is a
# crashed orphan, not a live task. 30 days is generous on both ends — it covers any
# realistic server downtime before recovery runs and any realistic long-running task
# for the badge — and it fully covers the 7-day FAILED_WORKTREE_QUARANTINE_DAYS band
# the recovery sweep consumes (a failed thread's metadata mtime == its completed_at
# write time), so no quarantine-eligible thread is ever skipped.
RUNNING_SCAN_WINDOW = timedelta(days=30)

# Parsed in-window thread metadata keyed by metadata path. The sidebar deep probe
# re-enters this scan on every poll that follows any write to the session, and
# re-parsing every unchanged metadata file dominated that probe (~25 ms measured on
# the 339-thread worst corpus); a repeat scan pays one stat per file and re-reads
# only files whose signature moved. Every thread-metadata writer publishes through
# write_model_json_atomically's tmp-file rename, so any content change moves
# mtime_ns and an unchanged (mtime_ns, size) proves the content current (the
# stat-before-read race contract is StatSignatureMemo's). Yielded dicts are shared
# across calls and scans — consumers must treat them as read-only.
_THREAD_META_MEMO_LIMIT = 1024
_thread_meta_memo: StatSignatureMemo[str, dict] = StatSignatureMemo(_THREAD_META_MEMO_LIMIT)


def _iter_thread_meta_stats(threads_dir: Path, log_event: str) -> Iterator[tuple[str, str, os.stat_result]]:
  """Yield ``(thread_dir, metadata.json path, stat)`` for every thread dir under *threads_dir*.

  The one scandir+stat walk both stat-first consumers take: the signature walk
  materializes it into a list, the recent-meta scan filters it in flight.
  Lazy, so a consumer may short-circuit mid-walk. Thread dirs without a
  readable ``metadata.json`` are skipped (mid-creation races have nothing to
  read); other stat failures log *log_event* and skip.
  """
  if not threads_dir.is_dir():
    return
  with os.scandir(threads_dir) as entries:
    for entry in entries:
      if not entry.is_dir():
        continue
      # entry.path is the str join scandir already built; appending "/{METADATA_NAME}"
      # directly yields the same string Path(entry.path) / METADATA_NAME would.
      meta_path = f"{entry.path}/{METADATA_NAME}"
      try:
        st = os.stat(meta_path)
      except FileNotFoundError:
        continue  # thread dir without metadata.json (mid-creation) — nothing to read
      except OSError as e:
        log.debug(log_event, path=meta_path, error=str(e))
        continue
      yield entry.path, meta_path, st


# The signature walk's dir-listing memo: (threads dir path, its st_mode) ->
# the thread-dir and metadata.json path strings, signed on the directory's
# (mtime_ns, size). Creating or removing a thread dir always moves the listing
# directory's own mtime, so the fresh per-metadata stat below stays the only
# content witness; only the scandir+is_dir+join phase rides the memo, the phase
# the sidebar's 10th-poll sweep repeats for every active session. The mode
# rides the key so a permission change misses into the scandir's own error.
_THREAD_WALK_DIR_MEMO_LIMIT = 1024
_thread_walk_dirs: StatSignatureMemo[tuple[str, int],
                                     list[tuple[str, str]]] = StatSignatureMemo(_THREAD_WALK_DIR_MEMO_LIMIT)


def walk_thread_meta_stats(threads_dir: Path, log_event: str) -> list[tuple[str, str, os.stat_result]]:
  """``(thread_dir, metadata.json path, stat)`` for every thread dir under *threads_dir*.

  The scandir+stat phase the sidebar probe's signature walk takes once and
  hands to ``iter_recent_thread_metas``' walked branch. The name pairs serve
  from the directory-state memo while the directory's stat pair holds; every
  metadata.json stat is taken fresh per call.
  """
  dir_key = os.fspath(threads_dir)
  try:
    dir_st = os.stat(dir_key)
  except OSError:
    return []
  if not stat.S_ISDIR(dir_st.st_mode):
    return []
  memo_key = (dir_key, dir_st.st_mode)
  names = _thread_walk_dirs.fresh(memo_key, dir_st)
  if names is None:
    with os.scandir(dir_key) as entries:
      names = [(entry.path, f"{entry.path}/{METADATA_NAME}") for entry in entries if entry.is_dir()]
    _thread_walk_dirs.record(memo_key, dir_st, names)
  rows: list[tuple[str, str, os.stat_result]] = []
  for thread_dir, meta_path in names:
    try:
      st = os.stat(meta_path)
    except FileNotFoundError:
      continue  # thread dir without metadata.json (mid-creation) — nothing to read
    except OSError as error:
      log.debug(log_event, path=meta_path, error=str(error))
      continue
    rows.append((thread_dir, meta_path, st))
  return rows


def iter_recent_thread_metas(
    threads_dir: Path,
    now: datetime,
    log_event: str,
    walked: list[tuple[str, str, os.stat_result]] | None = None,
) -> Iterator[tuple[str, str, dict]]:
  """Yield ``(thread_dir, meta_path, meta)`` for threads modified within RUNNING_SCAN_WINDOW.

  Cheap-first: ``os.scandir`` the threads dir and ``os.stat`` each ``metadata.json``,
  only ``load_json_meta`` (read + parse) the ones whose mtime is at least
  ``now - RUNNING_SCAN_WINDOW``. Threads whose metadata is older than the window are skipped
  with zero content reads, as are dirs with missing/unreadable metadata. In-window
  parses are memoized on (mtime_ns, size) (see the memo above the scan's callers),
  so a repeat scan over unchanged files costs one stat per file. Shared by
  ``_scan_thread_metas`` (init) and ``has_running_tasks_sync`` (sessions) so the
  stat-before-read scan stays identical at both sites.

  *walked* replaces the scandir+stat phase with stat results a caller already
  took over the same corpus — the sidebar's deep probe passes the probe-input
  signature walk's pairs so one post-write poll walks the thread dirs once, and
  the probe result is stored with that same walk's signature.

  The yielded paths and the stat go through scandir's plain strings, not Path
  objects: the sidebar deep probe re-enters this scan on every post-write poll,
  and the Path allocations measured over half the scan's cost on the 339-thread
  worst corpus (the same finding the sidebar signature pass fixed).
  """
  triples = walked if walked is not None else _iter_thread_meta_stats(threads_dir, log_event)
  cutoff = (now - RUNNING_SCAN_WINDOW).timestamp()
  for thread_dir, meta_path, st in triples:
    if st.st_mtime < cutoff:
      continue
    meta = _recent_thread_meta(meta_path, st, log_event)
    if meta is not None:
      yield thread_dir, meta_path, meta


def _recent_thread_meta(meta_path: str, st: os.stat_result, log_event: str) -> dict | None:
  """Parsed metadata for *meta_path* whose stat is *st*: memo hit or one read+parse."""
  meta = _thread_meta_memo.fresh(meta_path, st)
  if meta is not None:
    return meta
  meta = load_json_meta(Path(meta_path), log_event)
  if meta is None:
    return None
  _thread_meta_memo.record(meta_path, st, meta)
  return meta


def _scan_thread_metas(cfg: CharlieBotConfig) -> list[dict]:
  """Collect the in-window thread metadata list the quarantine sweep consumes.

  The thread-recovery half (reconcile/respawn/drain of pre-boot threads) is
  gone — legacy threads are read-only records and their executor no longer
  exists. The scan remains for the startup sweep: only threads whose
  ``metadata.json`` mtime falls within ``RUNNING_SCAN_WINDOW`` are read+parsed
  (via ``iter_recent_thread_metas``), which soundly covers the 7-day
  ``FAILED_WORKTREE_QUARANTINE_DAYS`` band (a failed thread's metadata mtime
  equals its ``completed_at`` write time). Threads of a task-tree (v2) session
  are skipped: they never had ThreadManager rows.
  """
  if not cfg.sessions_dir.exists():
    return []
  threads: list[dict] = []
  now = utc_now()
  for session_dir in cfg.sessions_dir.iterdir():
    threads_dir = session_dir / THREADS_DIR_NAME
    if not threads_dir.is_dir():
      continue
    if _session_is_task_tree(session_dir):
      continue
    for _thread_dir, _meta_path, meta in iter_recent_thread_metas(threads_dir, now, "thread_meta_unreadable"):
      threads.append(meta)
  return threads


def _liveness_probe(pid: int | None, pid_start: str | None, started_at: datetime | None,
                    host_boot: datetime) -> Callable[[], bool]:
  """The liveness probe a boot re-attach mounts with, by input completeness.

  A missing input (pid/pid_start/started_at) makes death unprovable, so the
  probe is constant-true: the follow waits for the run's real result event
  instead of ever judging death from incomplete evidence. The master branch's
  re-attach (init_master_recovery) mounts this one rule.
  """
  if pid is None or pid_start is None or started_at is None:
    return lambda: True
  return runs.run_alive_probe(pid, pid_start, started_at, host_boot)


async def _report_recovery_event(session_mgr: SessionManager, session_id: str, content: str) -> None:
  """Persist a user-visible recovery report to the session chat stream."""
  try:
    await session_mgr.deliver_to_successor(
        session_id, {
            "type": ET.ERROR,
            "content": content,
            "source": "crash_recovery",
        })
    await session_mgr.mark_unread(session_id)
  except Exception as e:
    log.warning("recovery_report_failed", session=session_id, error=str(e))


def _session_is_task_tree(session_dir: Path) -> bool:
  """Whether the session directory belongs to a task-tree (v2) node.

  Read from the metadata file directly (one cheap read per session with a
  threads directory); unreadable metadata means "not provably v2", and the
  scan then behaves exactly as before.
  """
  from src.core.json_utils import load_json_meta
  raw = load_json_meta(session_dir / METADATA_NAME, "thread_scan_meta_unreadable")
  return raw is not None and bool(raw.get("profile"))


async def _quarantine_stale_failed_worktrees(cfg: CharlieBotConfig, threads: list[dict]) -> None:
  """Move worktrees of long-failed threads into the trash dir. Best-effort, never raises.

  Driven purely by thread metadata (the trash dir is never re-scanned as worktrees).
  A failed thread's worktree is quarantined only when every one of these holds:
    - keep_worktree is not set;
    - repo_path, worktree_path, and branch_name are all present;
    - completed_at is present and parseable, and older than the age threshold;
    - the worktree path still exists (idempotent across restarts);
    - no non-terminal (idle/running) thread still references the same worktree_path.
  Results are deduped by worktree_path so a shared worker+reviewer tree moves once.

  *threads* holds only metadata within ``RUNNING_SCAN_WINDOW`` (30 days), which by
  construction covers the full 7-day ``FAILED_WORKTREE_QUARANTINE_DAYS`` band: a
  failed thread's metadata mtime equals its ``completed_at``, so every quarantine
  candidate (completed_at 7–30 days ago) is in the list the recovery scan produced.
  """
  worktree_parent = Path(cfg.paths.worktree_dir)
  trash_path = trash_dir(cfg.paths.worktree_dir)

  active_worktrees = {
      meta.get("worktree_path")
      for meta in threads
      if meta.get("worktree_path") and meta.get("status") not in TERMINAL_THREAD_STATUSES
  }

  now = utc_now()
  cutoff = timedelta(days=FAILED_WORKTREE_QUARANTINE_DAYS)
  swept: set[str] = set()
  for meta in threads:
    try:
      if meta.get("status") != "failed" or meta.get("keep_worktree"):
        continue
      repo_path = meta.get("repo_path")
      worktree_path = meta.get("worktree_path")
      branch_name = meta.get("branch_name")
      if not (repo_path and worktree_path and branch_name) or worktree_path in swept:
        continue
      completed_at = meta.get("completed_at")
      if not completed_at:
        continue
      try:
        completed_dt = parse_utc_datetime(completed_at)
      except (ValueError, TypeError):
        log.warning("quarantine_skip_unparseable_completed_at", thread=meta.get("id"), completed_at=completed_at)
        continue
      if now - completed_dt < cutoff:
        continue
      wt = Path(worktree_path)
      if not wt.exists():
        continue
      if worktree_path in active_worktrees:
        log.warning("quarantine_skip_active_worktree", thread=meta.get("id"), worktree=worktree_path)
        continue
      swept.add(worktree_path)
      try:
        await git_quarantine_worktree(
            repo_path,
            wt,
            meta.get("id") or wt.name,
            allowed_parent=worktree_parent,
            expected_residue_name=git_worktree_dir_name(branch_name),
            trash_dir=trash_path,
        )
      except Exception as e:
        log.exception("quarantine_worktree_failed", thread=meta.get("id"), worktree=worktree_path, error=str(e))
    except Exception:
      log.exception("quarantine_thread_sweep_failed", thread=meta.get("id"))
      continue

  if trash_path.exists():
    # dir_size_bytes walks ~18.5k trash entries; run it off the event loop so the
    # synchronous os.walk does not block live request serving during recovery.
    total = await asyncio.to_thread(dir_size_bytes, trash_path)
    log.info("worktree_trash_size", path=str(trash_path), bytes=total, human=format_size(total))
