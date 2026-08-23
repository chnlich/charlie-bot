"""Worker-side crash recovery: pre-boot thread scan, reconcile, and worktree quarantine."""

import asyncio
import os
import signal
from collections.abc import Callable, Iterator
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Optional

import structlog

from src.core import finalize_effects, runs
from src.core.git import git_quarantine_worktree, git_worktree_dir_name
from src.core.json_utils import load_json_meta
from src.core.models import (
  TERMINAL_THREAD_STATUSES,
  TaskType,
  ThreadMetadata,
  parse_utc_datetime,
  utc_now,
)
from src.core.process import kill_process_group
from src.core.tasks import create_logged_task
from src.core.timeouts import NO_OUTPUT_REPORT_THRESHOLD
from src.core.worktree_trash import dir_size_bytes, format_size, trash_dir

log = structlog.get_logger()

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

# Boot-scoped once-key for "alive but silent" reports: at most one recovery
# event per thread per boot, shared by the boot STALLED report and the
# follow-time silence recheck (whichever emits first claims the key). Process
# lifetime only — deliberately nothing on disk.
_silence_reported_thread_ids: set[str] = set()


def iter_recent_thread_metas(
    threads_dir: Path,
    now: datetime,
    log_event: str,
    window: timedelta = RUNNING_SCAN_WINDOW,
) -> Iterator[tuple[Path, Path, dict]]:
  """Yield ``(thread_dir, meta_path, meta)`` for threads modified within *window*.

  Cheap-first: ``os.scandir`` the threads dir and ``stat`` each ``metadata.json``,
  only ``load_json_meta`` (read + parse) the ones whose mtime is at least
  ``now - window``. Threads whose metadata is older than the window are skipped
  with zero content reads, as are dirs with missing/unreadable metadata. Shared by
  ``_recover_orphaned_threads`` (init) and ``_has_running_tasks`` (sessions) so the
  stat-before-read scan stays identical at both sites.
  """
  if not threads_dir.is_dir():
    return
  cutoff = (now - window).timestamp()
  with os.scandir(threads_dir) as entries:
    for entry in entries:
      if not entry.is_dir():
        continue
      meta_path = Path(entry.path) / "metadata.json"
      try:
        mtime = meta_path.stat().st_mtime
      except FileNotFoundError:
        continue  # thread dir without metadata.json (mid-creation) — nothing to read
      except OSError as e:
        log.debug(log_event, path=str(meta_path), error=str(e))
        continue
      if mtime < cutoff:
        continue
      meta = load_json_meta(meta_path, log_event)
      if meta is None:
        continue
      yield Path(entry.path), meta_path, meta


@dataclass(frozen=True)
class _InterruptedRun:
  """A pre-boot thread needing reconciliation: its identity and raw metadata."""
  session_id: str
  thread_dir: Path
  meta: dict



def _scan_interrupted_runs(cfg, boot_time: datetime) -> tuple[list[_InterruptedRun], list[dict]]:
  """Collect pre-boot threads (any status) plus the full in-window metadata list.

  Only threads whose ``metadata.json`` mtime falls within ``RUNNING_SCAN_WINDOW``
  are read+parsed (via ``iter_recent_thread_metas``); older thread dirs are skipped
  entirely. This is sound because a live thread's metadata is always recent, and
  the 30-day read window fully covers the 7-day quarantine band consumed by
  ``_quarantine_stale_failed_worktrees`` (a failed thread's metadata mtime equals
  its ``completed_at`` write time).

  Returns (interrupted runs, all in-window metadata dicts) — the second list
  feeds the quarantine sweep unchanged.
  """
  if not cfg.sessions_dir.exists():
    return [], []
  interrupted: list[_InterruptedRun] = []
  threads: list[dict] = []
  now = utc_now()
  for session_dir in cfg.sessions_dir.iterdir():
    threads_dir = session_dir / "threads"
    for thread_dir, _meta_path, meta in iter_recent_thread_metas(threads_dir, now, "thread_meta_unreadable"):
      if _started_before_boot(meta, thread_dir, boot_time):
        interrupted.append(_InterruptedRun(session_id=session_dir.name, thread_dir=thread_dir, meta=meta))
      threads.append(meta)
  return interrupted, threads


def _translate_for_thread(cfg, meta: dict):
  """A fresh translate_event callable for resolving a run's raw log.

  Stateful translates (codex text buffering, gemini) require one instance per
  whole-file scan. An unknown/stale backend id degrades to the identity
  translate (raw claude shape) rather than failing the reconcile pass.
  """
  backend_id = meta.get("backend")
  if backend_id:
    try:
      from src.agents.backends.registry import build_backend
      from src.core.spawner import resolve_backend_option
      option = resolve_backend_option(cfg, backend_id, meta.get("model"))
      return build_backend(option, cfg).translate_event
    except Exception as e:
      log.warning("reconcile_translate_unresolved", thread=meta.get("id"), backend=backend_id, error=str(e))
  return lambda event: [event]


async def _reconcile_interrupted_runs(cfg, session_mgr, thread_mgr, interrupted: list[_InterruptedRun]) -> int:
  """Resolve each pre-boot thread's run truth and dispatch its recovery action."""
  if not interrupted:
    return 0
  from src.core import (
    spawner,  # lazy: spawner imports sessions, sessions imports this module
  )

  host_boot = await asyncio.to_thread(runs.read_host_boot_time)
  holders_scan = await asyncio.to_thread(runs.scan_stdout_holders)
  chat_cache: dict[str, list[dict]] = {}
  threads_cache: dict[str, list] = {}
  recovered = 0
  for item in interrupted:
    try:
      if item.session_id not in chat_cache:
        chat_cache[item.session_id] = await asyncio.to_thread(session_mgr.load_chat_events_sync, item.session_id)
      if item.session_id not in threads_cache:
        threads_cache[item.session_id] = await thread_mgr.list_threads(item.session_id)
      if await _reconcile_one(
          cfg,
          session_mgr,
          thread_mgr,
          spawner,
          item,
          host_boot=host_boot,
          holders_scan=holders_scan,
          chat_events=chat_cache[item.session_id],
          session_threads=threads_cache[item.session_id],
      ):
        recovered += 1
    except Exception:
      log.exception("reconcile_run_failed", thread=item.meta.get("id"), session=item.session_id)
  if recovered:
    log.info("interrupted_run_reconcile_done", count=recovered)
  return recovered


def _liveness_probe(pid, pid_start, started_at, host_boot: datetime) -> Callable[[], bool]:
  """The liveness probe a boot re-attach mounts with, by input completeness.

  A missing input (pid/pid_start/started_at) makes death unprovable, so the
  probe is constant-true: the follow waits for the run's real result event
  instead of ever judging death from incomplete evidence. Complete inputs get
  the recorded (pid, pid_start) identity judgment. Worker and master branches
  share this one rule.
  """
  if pid is None or pid_start is None or started_at is None:
    return lambda: True
  return lambda: runs.is_run_alive(pid, pid_start, started_at, host_boot)


async def _reconcile_one(
    cfg,
    session_mgr,
    thread_mgr,
    spawner,
    item: _InterruptedRun,
    *,
    host_boot: datetime,
    holders_scan: dict,
    chat_events: list[dict],
    session_threads: list,
) -> bool:
  """Dispatch one interrupted thread; returns True when a recovery task started."""
  meta = item.meta
  thread_id = meta.get("id")
  session_id = item.session_id
  description = meta.get("description", "")

  if meta.get("status") in TERMINAL_THREAD_STATUSES:
    await _complete_finalize_effects(
        cfg, session_mgr, thread_mgr, spawner, item,
        chat_events=chat_events, session_threads=session_threads)
    return False

  resolution = runs.resolve_run(
      raw_path=runs.raw_log_path(item.thread_dir),
      pid=meta.get("pid"),
      pid_start=meta.get("pid_start"),
      started_at=_parse_started_at(meta),
      backend_type=runs.backend_type(cfg, meta.get("backend")),
      translate=_translate_for_thread(cfg, meta),
      host_boot_time=host_boot,
      holders_scan=holders_scan,
  )
  log.info(
      "reconcile_run_resolved",
      thread=thread_id,
      session=session_id,
      outcome=resolution.outcome.value,
      reason=resolution.reason,
      leftover=len(resolution.leftover_holders),
  )

  if resolution.outcome is runs.RunOutcome.NEVER_STARTED:
    respawned = await _maybe_respawn(
        cfg, session_mgr, thread_mgr, spawner, item, chat_events=chat_events)
    if respawned:
      return True
    # Reviewers are replaced by the retry chain, improve iterations by nothing
    # (loop continuation is a non-goal): drain-finalize them as failed.
    create_logged_task(
        spawner.resume_worker(session_id, description, thread_id, cfg, session_mgr, thread_mgr,
                              is_alive=lambda: False, interrupt_reason="", on_silence=None),
        name=f"resume-drain-{thread_id[:8]}")
    return True

  if resolution.outcome in (runs.RunOutcome.RUNNING, runs.RunOutcome.STALLED):
    if resolution.outcome is runs.RunOutcome.RUNNING and resolution.reason in (
        runs.UNCOVERED_ALIVE_REASON, runs.RAW_MISSING_ALIVE_REASON):
      # Treated as alive but nothing followable (uncovered transport / no raw
      # log): report only — no re-attach, nothing torn down, judged again on
      # the next restart.
      await _report_recovery_event(
          session_mgr,
          session_id,
          f"Worker thread {thread_id[:8]} is treated as still alive ({resolution.reason}); "
          "it is NOT being killed — left running without re-attach and judged again on "
          "the next restart.",
      )
      return True
    if resolution.outcome is runs.RunOutcome.STALLED:
      # The boot report claims this thread's one-per-boot silence report.
      _silence_reported_thread_ids.add(thread_id)
      await _report_recovery_event(
          session_mgr,
          session_id,
          f"Worker thread {thread_id[:8]} is still alive but produced no output for over "
          f"{NO_OUTPUT_REPORT_THRESHOLD // 3600}h ({resolution.reason}). Suspected hung; "
          "it is NOT being killed — re-attached and left running.",
      )
    create_logged_task(
        spawner.resume_worker(
            session_id,
            description,
            thread_id,
            cfg,
            session_mgr,
            thread_mgr,
            is_alive=_liveness_probe(meta.get("pid"), meta.get("pid_start"), _parse_started_at(meta), host_boot),
            interrupt_reason="",
            on_silence=lambda: _follow_silence_recheck(session_mgr, session_id, thread_id),
        ),
        name=f"resume-follow-{thread_id[:8]}")
    return True

  # COMPLETED / DIED: drain whatever events were not consumed, then finalize.
  # DIED carries resolve_run's reason (transport not covered, no result event,
  # …) into the finalize error, so the master's summary states why the run
  # failed instead of a bare exit -1; COMPLETED has no reason and is unaffected.
  create_logged_task(
      spawner.resume_worker(session_id, description, thread_id, cfg, session_mgr, thread_mgr,
                            is_alive=lambda: False, interrupt_reason=resolution.reason, on_silence=None),
      name=f"resume-drain-{thread_id[:8]}")

  # Row 5: descendants that outlived the run while holding its raw-log fd are
  # killed via the existing kill_process_group and named in the report.
  if resolution.leftover_holders:
    named = ", ".join(f"pid {h.pid} ({h.cmdline or 'unknown command'})" for h in resolution.leftover_holders)
    for holder in resolution.leftover_holders:
      kill_process_group(holder.pid, signal.SIGTERM)
    await _report_recovery_event(
        session_mgr,
        session_id,
        f"Worker thread {thread_id[:8]} ended but left descendant process(es) holding its output "
        f"log; terminated: {named}.",
    )
  return True


def _parse_started_at(meta: dict) -> Optional[datetime]:
  started_at = meta.get("started_at")
  if not started_at:
    return None
  try:
    return parse_utc_datetime(started_at)
  except (ValueError, TypeError):
    return None


async def _report_recovery_event(session_mgr, session_id: str, content: str) -> None:
  """Persist a user-visible recovery report to the session chat stream."""
  try:
    await session_mgr.deliver_to_successor(
        session_id, {
            "type": "error",
            "content": content,
            "source": "crash_recovery",
        })
    await session_mgr.mark_unread(session_id)
  except Exception as e:
    log.warning("recovery_report_failed", session=session_id, error=str(e))


async def _follow_silence_recheck(session_mgr, session_id: str, thread_id: str) -> None:
  """Emit this thread's one-per-boot silence report from a mounted follow.

  Same text shape as the boot STALLED report, and shares its once-key:
  whichever side emits first claims the thread for this boot, so repeated
  re-mounts within one boot can never re-emit.
  """
  if thread_id in _silence_reported_thread_ids:
    return
  _silence_reported_thread_ids.add(thread_id)
  await _report_recovery_event(
      session_mgr,
      session_id,
      f"Worker thread {thread_id[:8]} is still alive but produced no output for over "
      f"{NO_OUTPUT_REPORT_THRESHOLD // 3600}h (silence crossed after re-attach). Suspected hung; "
      "it is NOT being killed — re-attached and left running.")


async def _maybe_respawn(
    cfg,
    session_mgr,
    thread_mgr,
    spawner,
    item: _InterruptedRun,
    *,
    chat_events: list[dict],
) -> bool:
  """Respawn a registered-but-never-started worker from its persisted invocation.

  Returns False (caller falls back to drain-finalize) for reviewers — the
  retry chain replaces them — and for improve iterations and threads whose
  delegation invocation is not found in the chat log (not reconstructible).
  """
  from src.core.models import SpawnRequest

  meta = item.meta
  thread_id = meta.get("id")
  session_id = item.session_id
  description = meta.get("description", "")
  if meta.get("review_of") or description.startswith(runs.IMPROVE_ITERATION_PREFIX):
    return False

  invocation = None
  for ev in reversed(chat_events):
    if ev.get("type") == "task_delegated" and ev.get("thread_id") == thread_id:
      invocation = ev.get("delegate_invocation") or {}
      break
  if invocation is None:
    log.warning("respawn_invocation_missing", thread=thread_id, session=session_id)
    return False

  request_task_type = TaskType(invocation.get("task_type") or meta.get("task_type") or TaskType.IMPLEMENT)
  requested_backend = invocation.get("backend")
  try:
    if request_task_type == TaskType.VERIFY and requested_backend is None:
      resolved_backend, resolved_model, _ = await spawner.select_verify_backend(session_id, cfg, session_mgr, [])
    else:
      resolved_backend, resolved_model = await spawner.resolve_requested_subagent_backend_model(
          session_id, cfg, session_mgr, requested_backend=requested_backend)
  except ValueError as e:
    log.error("respawn_backend_unresolved", thread=thread_id, error=str(e))
    return False

  # Crash between worktree creation and spawn: reuse the persisted worktree
  # and branch; their absence simply takes the fresh-creation path.
  worktree_path_override = meta.get("worktree_path")
  request = SpawnRequest(
      repo_path=invocation.get("repo_path") or meta.get("repo_path"),
      base_branch=invocation.get("base_branch") or meta.get("base_branch"),
      context=meta.get("context"),
      resolved_backend=resolved_backend,
      resolved_model=resolved_model,
      branch_name_override=meta.get("branch_name"),
      worktree_path_override=worktree_path_override,
      keep_worktree=bool(invocation.get("keep_worktree", False)),
      task_type=request_task_type,
  )
  log.warning("respawning_never_started_worker", thread=thread_id, session=session_id)
  create_logged_task(
      spawner.spawn_worker(session_id, description, thread_id, cfg, session_mgr, thread_mgr, request=request),
      name=f"respawn-worker-{thread_id[:8]}")
  return True


async def _complete_finalize_effects(
    cfg,
    session_mgr,
    thread_mgr,
    spawner,
    item: _InterruptedRun,
    *,
    chat_events: list[dict],
    session_threads: list,
) -> None:
  """Re-run the finalize EFFECTS of a terminally-marked thread when any is missing.

  A crash between the terminal-status write and the notify chain leaves a
  thread marked completed/failed with effects pending. Each effect is
  judgment-idempotent (see finalize_effects), so re-running converges to a
  no-op; a thread with all effects present is skipped cheaply here. Improve
  iterations are excluded (skip_notify semantics died with the loop).
  """
  meta = item.meta
  thread_id = meta.get("id")
  session_id = item.session_id
  description = meta.get("description", "")
  if description.startswith(runs.IMPROVE_ITERATION_PREFIX):
    return
  if not _effects_maybe_missing(meta, chat_events, session_threads):
    return
  log.warning("recompleting_finalize_effects", thread=thread_id, session=session_id, status=meta.get("status"))
  exit_code = meta.get("exit_code")
  create_logged_task(
      spawner.recomplete_finalize_effects(
          session_id,
          description,
          ThreadMetadata(**meta),
          exit_code if exit_code is not None else -1,
          thread_mgr,
          session_mgr,
          cfg,
          task_type=TaskType(meta.get("task_type") or TaskType.IMPLEMENT),
      ),
      name=f"recomplete-finalize-{thread_id[:8]}")


def _effects_maybe_missing(meta: dict, chat_events: list[dict], session_threads: list) -> bool:
  """Cheap judgment scan: any of the thread's three finalize effects absent?"""
  thread_id = meta.get("id")
  if not finalize_effects.terminal_summary_present(chat_events, thread_id):
    return True
  if not finalize_effects.master_woke_after_summary(chat_events, thread_id):
    return True
  if (
      meta.get("status") == "completed"
      and meta.get("require_review", True)
      and not meta.get("review_of")
      and meta.get("repo_path")
      and meta.get("branch_name")
      and meta.get("worktree_path")
  ):
    return not finalize_effects.reviewer_thread_exists(session_threads, thread_id, exclude_thread_id=thread_id)
  return False


def _started_before_boot(meta: dict, thread_dir: Path, boot_time: datetime) -> bool:
  """Return True if a running thread was started before this server boot.

  A genuine post-boot worker always carries a fresh started_at, so anything
  earlier than *boot_time* is orphaned. When started_at is missing or
  unparseable, fall back to the thread directory's ctime; if even that is
  unavailable, treat the thread as pre-boot and recover it (errs safe).
  """
  started_at = meta.get("started_at")
  if started_at:
    try:
      return parse_utc_datetime(started_at) < boot_time
    except (ValueError, TypeError):
      log.warning("recover_unparseable_started_at", thread=meta.get("id"), started_at=started_at)
  try:
    ctime = datetime.fromtimestamp(thread_dir.stat().st_ctime, tz=timezone.utc)
  except OSError:
    return True
  return ctime < boot_time


async def _quarantine_stale_failed_worktrees(cfg, threads: list[dict]) -> None:
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
  worktree_parent = Path(cfg.worktree_dir)
  trash_path = trash_dir(cfg.worktree_dir)

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
        log.error(
            "quarantine_worktree_failed", thread=meta.get("id"), worktree=worktree_path, error=str(e), exc_info=True)
    except Exception:
      log.exception("quarantine_thread_sweep_failed", thread=meta.get("id"))
      continue

  if trash_path.exists():
    # dir_size_bytes walks ~18.5k trash entries; run it off the event loop so the
    # synchronous os.walk does not block live request serving during recovery.
    total = await asyncio.to_thread(dir_size_bytes, trash_path)
    log.info("worktree_trash_size", path=str(trash_path), bytes=total, human=format_size(total))
