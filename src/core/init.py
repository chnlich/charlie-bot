"""Initialize ~/.charliebot/ directory structure on first run."""

import asyncio
import copy
import os
import signal
import subprocess
from collections.abc import Iterator
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Optional

import structlog
import yaml

from src.core import event_types as ET
from src.core import finalize_effects, runs
from src.core.config import (
  CharlieBotConfig,
  ScheduledTaskConfig,
  _resolve_local_timezone,
  _resolve_prompt_file,
  get_config,
)
from src.core.git import git_quarantine_worktree, git_worktree_dir_name
from src.core.json_utils import load_json_meta
from src.core.models import TaskType, ThreadMetadata, parse_utc_datetime, utc_now
from src.core.process import kill_process_group
from src.core.tasks import create_logged_task
from src.core.timeouts import NO_OUTPUT_REPORT_THRESHOLD
from src.core.worktree_trash import dir_size_bytes, format_size, trash_dir
from src.core.yaml_utils import load_yaml, save_yaml

log = structlog.get_logger()

# Failed worktrees older than this are swept into <worktree_dir>/.trash/ on startup.
# Kept (not hard-deleted) so recent failures stay available for debugging.
FAILED_WORKTREE_QUARANTINE_DAYS = 7

# Thread statuses that mean the worker is done; anything else still owns its worktree.
_TERMINAL_THREAD_STATUSES = frozenset({"completed", "failed", "cancelled"})

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


def _default_config_yaml() -> dict:
  """Build the default config dict with placeholder values."""
  return {
      "workspace_dirs": ["~/workspace"],
      "worktree_dir": "~/worktrees",
  }


DEFAULT_MEMORY_TOPICS = (
    "profile resident\n"
    "communication resident\n"
    "workflow resident\n"
    "rulings resident\n"
    "host resident\n"
    "charliebot\n")

DEFAULT_MEMORY_GITIGNORE = "staging/\n"

DEFAULT_SLASH_COMMANDS = """\
commands:
  # Initially empty — /help is built-in, not defined here.
  #
  # Example shell command:
  # git:
  #   scope: shell
  #   description: "Run git command"
  #   args: "<git args>"
  #   command: "git {args}"
  #   cwd: "/path/to/repo"
  #   timeout: 10
  #
  # Example prompt command:
  # summarize:
  #   scope: prompt
  #   description: "Summarize conversation"
  #   prompt: "Summarize our conversation in bullet points."
  #
  # Example prompt command with claude_code_flags (runs in plan-only mode):
  # plan:
  #   scope: prompt
  #   description: 'Plan without implementing'
  #   args: '<what to plan>'
  #   prompt: '{args}'
  #   claude_code_flags: ['--permission-mode', 'plan']
"""


async def init_charliebot_home() -> None:
  """Ensure ~/.charliebot/ directory structure exists and seed default files."""
  cfg = get_config()

  # Create all required directories
  dirs = [
      cfg.charliebot_home,
      cfg.sessions_dir,
      cfg.config_d_dir,
  ]
  for d in dirs:
    d.mkdir(parents=True, exist_ok=True)

  # Seed the memory store scaffold (git repo + topics vocabulary + .gitignore)
  _seed_memory_scaffold(cfg)
  _seed_if_missing(cfg.charliebot_home / 'slash_commands.yaml', DEFAULT_SLASH_COMMANDS)

  # Seed config.yaml from the committed template if missing
  if not cfg.config_file.exists():
    template = cfg.charlie_bot_repo / "configs" / "config.example.yaml"
    if template.exists():
      cfg.config_file.write_text(template.read_text(encoding="utf-8"), encoding="utf-8")
    else:
      with open(cfg.config_file, "w") as f:
        yaml.dump(_default_config_yaml(), f, default_flow_style=False, sort_keys=False)


def _seed_if_missing(path: Path, content: str) -> None:
  """Write content to path only if the file does not already exist."""
  if not path.exists():
    path.write_text(content, encoding="utf-8")


def _seed_memory_scaffold(cfg: CharlieBotConfig) -> None:
  """Seed the labeled-entry memory store at cfg.memory_dir (idempotent).

  Creates the directory, runs ``git init`` when it is not already a repo, seeds
  the topics vocabulary and .gitignore (never overwriting existing files), and
  creates the ``entries/`` and ``staging/`` directories. The canon (entries/
  and topics) is populated only by user-approved curation diffs, never here.
  """
  memory_dir = cfg.memory_dir
  memory_dir.mkdir(parents=True, exist_ok=True)
  if not (memory_dir / ".git").exists():
    subprocess.run(["git", "init"], cwd=str(memory_dir), check=True, capture_output=True)
  _seed_if_missing(memory_dir / "topics", DEFAULT_MEMORY_TOPICS)
  _seed_if_missing(memory_dir / ".gitignore", DEFAULT_MEMORY_GITIGNORE)
  (memory_dir / "entries").mkdir(exist_ok=True)
  (memory_dir / "staging").mkdir(exist_ok=True)


def seed_default_cron_tasks(cfg: CharlieBotConfig) -> list[dict]:
  """Seed repo-owned default cron tasks into per-job host files by name.

  Reads ``configs/cron.default.yaml`` from the repo and the host
  ``~/.charliebot/config.d/cron.d/`` directory. For each default entry: if no
  host file ``cron.d/<name>.yaml`` exists, create it with the entry body minus
  ``name``; if one exists, change nothing about it. Never rewrites or drops
  host-only files.

  Creates ``config.d/cron.d/`` when absent. Writes through
  :func:`src.core.yaml_utils.save_yaml` (the same writer ``src/api/cron.py``
  uses). Validates every default entry before writing: each must construct a
  :class:`ScheduledTaskConfig` after ``prompt_file``/``local`` resolution and
  every ``prompt_file`` must resolve to an existing file. Fails loudly without
  writing if validation fails, and fails loudly (writing nothing) when a legacy
  ``config.d/cron.yaml`` exists — migration to the per-job layout is a human
  action, never automatic.

  Returns a per-entry report: ``[{"name": str, "status": "created"|"exists"}, ...]``.

  This is a library function invoked only by ``./scripts/setup.sh``. It is NOT
  called by :func:`init_charliebot_home`, so the server startup path never
  writes cron config — that is an invariant the tests assert directly.
  """
  repo_root = cfg.charlie_bot_repo
  defaults_data = load_yaml(repo_root / "configs" / "cron.default.yaml", default={})
  default_entries = list(defaults_data.get("scheduled_tasks", []))

  # Validate every default entry on a resolved copy before touching the host
  # directory, so a bad repo default fails loudly without any partial write.
  for entry in default_entries:
    if not isinstance(entry, dict):
      raise ValueError(f"invalid default cron entry (not a mapping): {entry!r}")
    resolved = copy.deepcopy(entry)
    resolved.pop("name", None)
    _resolve_prompt_file(resolved, repo_root)  # raises ValueError
    _resolve_local_timezone(resolved)
    ScheduledTaskConfig(name=entry.get("name"), **resolved)  # raises on validation error

  # A leftover legacy cron.yaml is a loud tripwire, never a silent fallback:
  # refuse to seed (and write nothing) until a human migrates and removes it.
  legacy_path = cfg.config_d_dir / "cron.yaml"
  if legacy_path.exists():
    raise ValueError(
        f"legacy {legacy_path} present; not seeding. Split its entries into "
        f"config.d/cron.d/<name>.yaml and remove cron.yaml (migration is manual)")

  cron_d_dir = cfg.config_d_dir / "cron.d"
  cron_d_dir.mkdir(parents=True, exist_ok=True)

  report: list[dict] = []
  for entry in default_entries:
    name = entry.get("name")
    path = cron_d_dir / f"{name}.yaml"
    if path.exists():
      report.append({"name": name, "status": "exists"})
      continue
    body = {k: v for k, v in copy.deepcopy(entry).items() if k != "name"}
    save_yaml(path, body)
    report.append({"name": name, "status": "created"})
  return report


async def run_crash_recovery(cfg, boot_time: datetime, session_mgr=None, thread_mgr=None) -> int:
  """Reconcile interrupted runs and quarantine stale worktrees.

  This is the deferrable part of startup: the server lifespan launches it as a
  background task so readiness is not delayed by the O(history) per-thread
  metadata scan. The synchronous scans run via ``asyncio.to_thread`` so they
  never block the event loop while live requests are served; the async git
  quarantine calls and per-run dispatch are awaited normally.

  Reconciliation NEVER kills an interrupted run's recorded process: an
  interrupted run's truth is resolved from disk (raw log + pid/pid_start
  liveness) into the six-row outcome table, then either re-attached (still
  running), drained (finished while we were down), respawned (never started),
  or failed (died mid-run). Only leftover descendants holding the run's
  raw-log fd are killed, and they are named in a report. The finalize chain it
  dispatches into is judgment-idempotent, so a crash mid-finalize converges
  instead of duplicating.

  *boot_time* is captured at lifespan start. Only threads started before it are
  reconciled, so a worker spawned during the recovery window is spared.

  *session_mgr*/*thread_mgr* are injectable so the server passes its live
  instances; standalone callers (tests) get auto-constructed ones.

  Master-side reconciliation (:func:`_reconcile_master_runs`) runs after the
  worker-side pass: per session, a recorded live master turn is re-attached,
  a dead one clears its record, and every real user message after the last
  MASTER_DONE (minus the recorded turn's per-event exclusion) is replayed
  with a replay marker through the normal per-session queue.

  Returns the number of interrupted runs dispatched for recovery.
  """
  if session_mgr is None:
    from src.core.sessions import SessionManager
    session_mgr = SessionManager(cfg)
  if thread_mgr is None:
    from src.core.threads import ThreadManager
    thread_mgr = ThreadManager(cfg)
  interrupted, threads = await asyncio.to_thread(_scan_interrupted_runs, cfg, boot_time)
  recovered = await _reconcile_interrupted_runs(cfg, session_mgr, thread_mgr, interrupted)
  await _reconcile_master_runs(cfg, session_mgr, boot_time)
  await _quarantine_stale_failed_worktrees(cfg, threads)
  return recovered


@dataclass(frozen=True)
class _InterruptedRun:
  """A pre-boot thread needing reconciliation: its identity and raw metadata."""
  session_id: str
  thread_dir: Path
  meta: dict


# Improve-loop iteration threads are identified by their description prefix;
# the loop task itself does not survive a restart (loop continuation is an
# explicit non-goal), so these threads are finalized, never respawned.
_IMPROVE_ITERATION_PREFIX = "Iterative improvement — iteration"


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


def unanswered_user_events(chat_events: list[dict], exclude_ids: set[str]) -> list[dict]:
  """Real user events after the last MASTER_DONE, minus exclusions, in log order.

  Restart-replay contract: every real user event with no MASTER_DONE after it
  is unanswered and must be redelivered. Exclusions are per-event (never
  per-session): a recorded live turn excludes exactly its own user_event_id,
  and a live process excludes the events already sitting in its in-memory
  queue — everything else disappears with a killed process and is replayed.
  """
  last_done = -1
  for idx, ev in enumerate(chat_events):
    if ev.get("type") == ET.MASTER_DONE:
      last_done = idx
  return [
      ev for ev in chat_events[last_done + 1:]
      if ev.get("type") == ET.USER and isinstance(ev.get("content"), str)
      and ev.get("id") not in exclude_ids
  ]


async def _await_reattach(future: asyncio.Future) -> None:
  """Await a re-attached master turn so its recovery task stays alive to the end."""
  await future


async def _reconcile_master_runs(cfg, session_mgr, boot_time: datetime) -> None:
  """Resolve each session's recorded master turn, then replay unanswered user messages.

  Two ordered passes over active sessions:

  1. master_run records: a still-alive covered run is re-attached through the
     per-session queue (its user_event_id joins the replay exclusion set); a
     dead run — or one on an uncovered backend transport — clears its record
     so replay handles its message. Records spawned by this same process
     during the recovery window (started_at >= boot_time) are spared.
  2. replay: every real user event after the last MASTER_DONE, minus the
     per-event exclusions and this process's queued items, is redelivered
     with the replay marker. Pass 2 runs strictly after pass 1 enqueued the
     resume items, so a re-attached turn always drains before a replay spawns
     a new CLI against the same conversation.
  """
  from src.agents import master_cc  # lazy: mirrors the spawner import's cycle guard

  try:
    sessions = session_mgr.list_active_session_ids()
  except Exception:
    log.exception("master_reconcile_scan_failed")
    return
  host_boot = await asyncio.to_thread(runs.read_host_boot_time)
  excluded: dict[str, set[str]] = {}

  for meta in sessions:
    record = meta.master_run
    if record is None:
      continue
    if record.started_at >= boot_time:
      # This process spawned the turn during the recovery window — live and
      # self-owned: spare it, and keep its message out of the replay set.
      if record.user_event_id:
        excluded.setdefault(meta.id, set()).add(record.user_event_id)
      continue
    alive = runs.is_run_alive(record.pid, record.pid_start, record.started_at, host_boot)
    option = cfg.get_backend_option(meta.backend)
    covered = option is not None and option.type not in runs.UNCOVERED_BACKEND_TYPES
    if alive and covered:
      future = await master_cc.enqueue_master_resume(
          cfg,
          meta,
          record,
          session_mgr.callbacks(),
          is_alive=lambda r=record: runs.is_run_alive(r.pid, r.pid_start, r.started_at, host_boot),
      )
      create_logged_task(_await_reattach(future), name=f"master-resume-{meta.id[:8]}")
      if record.user_event_id:
        excluded.setdefault(meta.id, set()).add(record.user_event_id)
      log.info("master_run_reattached", session=meta.id, pid=record.pid, raw_log=record.raw_log)
    else:
      reason = runs.TRANSPORT_NOT_COVERED_REASON if alive else "recorded master process is dead"
      log.warning("master_run_cleared", session=meta.id, pid=record.pid, reason=reason)
      await session_mgr.persist_master_run(meta.id, None)

  for meta in sessions:
    try:
      events = session_mgr.load_chat_events_sync(meta.id)
      skip = excluded.get(meta.id, set()) | master_cc.queued_user_event_ids(meta.id)
      for ev in unanswered_user_events(events, skip):
        log.warning("master_replaying_user_message", session=meta.id, event_id=ev.get("id"))
        create_logged_task(
            master_cc.replay_user_message(cfg, meta, ev, session_mgr.callbacks()),
            name=f"master-replay-{meta.id[:8]}")
    except Exception:
      log.exception("master_replay_dispatch_failed", session=meta.id)


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

  if meta.get("status") in _TERMINAL_THREAD_STATUSES:
    await _complete_finalize_effects(
        cfg, session_mgr, thread_mgr, spawner, item,
        chat_events=chat_events, session_threads=session_threads)
    return False

  resolution = runs.resolve_run(
      raw_path=runs.raw_log_path(item.thread_dir),
      pid=meta.get("pid"),
      pid_start=meta.get("pid_start"),
      started_at=_parse_started_at(meta),
      backend_type=_backend_type(cfg, meta.get("backend")),
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
                              is_alive=lambda: False),
        name=f"resume-drain-{thread_id[:8]}")
    return True

  if resolution.outcome in (runs.RunOutcome.RUNNING, runs.RunOutcome.STALLED):
    if resolution.outcome is runs.RunOutcome.STALLED:
      await _report_recovery_event(
          session_mgr,
          session_id,
          f"Worker thread {thread_id[:8]} is still alive but produced no output for over "
          f"{NO_OUTPUT_REPORT_THRESHOLD // 3600}h ({resolution.reason}). Suspected hung; "
          "it is NOT being killed — re-attached and left running.",
      )
    pid = meta.get("pid")
    pid_start = meta.get("pid_start")
    started_at = _parse_started_at(meta)
    create_logged_task(
        spawner.resume_worker(
            session_id,
            description,
            thread_id,
            cfg,
            session_mgr,
            thread_mgr,
            is_alive=lambda pid=pid, pid_start=pid_start, started_at=started_at: runs.is_run_alive(
                pid, pid_start, started_at, host_boot),
        ),
        name=f"resume-follow-{thread_id[:8]}")
    return True

  # COMPLETED / DIED: drain whatever events were not consumed, then finalize.
  create_logged_task(
      spawner.resume_worker(session_id, description, thread_id, cfg, session_mgr, thread_mgr,
                            is_alive=lambda: False),
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


def _backend_type(cfg, backend_id: Optional[str]) -> Optional[str]:
  if not backend_id:
    return None
  option = cfg.get_backend_option(backend_id)
  return option.type if option else None


async def _report_recovery_event(session_mgr, session_id: str, content: str) -> None:
  """Persist a user-visible recovery report to the session chat stream."""
  try:
    await session_mgr.persist_and_broadcast(
        session_id, {
            "type": "error",
            "content": content,
            "source": "crash_recovery",
        })
    await session_mgr.mark_unread(session_id)
  except Exception as e:
    log.warning("recovery_report_failed", session=session_id, error=str(e))


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
  if meta.get("review_of") or description.startswith(_IMPROVE_ITERATION_PREFIX):
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
      # Mirrors _authorize_spawn_request's VERIFY branch (api/internal.py): verify
      # checks the session's work, so its backend defaults cross-model via
      # model_preference, exactly like the delegation reviewer.
      from src.core.review import select_reviewer_backend
      session_backend, session_model = await spawner.resolve_session_subagent_backend_model(
          session_id, cfg, session_mgr)
      resolved_backend, resolved_model, _ = select_reviewer_backend(cfg, session_backend, session_model, [])
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
  if description.startswith(_IMPROVE_ITERATION_PREFIX):
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
      if meta.get("worktree_path") and meta.get("status") not in _TERMINAL_THREAD_STATUSES
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
