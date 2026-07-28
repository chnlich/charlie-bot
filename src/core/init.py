"""Initialize ~/.charliebot/ directory structure on first run."""

import asyncio
import copy
import json
import os
import signal
from collections.abc import Iterator
from datetime import datetime, timedelta, timezone

import yaml
from pathlib import Path

import structlog

from src.core.config import (
    CharlieBotConfig,
    ScheduledTaskConfig,
    _resolve_local_timezone,
    _resolve_prompt_file,
    get_config,
)
from src.core.git import git_quarantine_worktree, git_worktree_dir_name
from src.core.json_utils import load_json_meta
from src.core.models import parse_utc_datetime, utc_now
from src.core.process import kill_process_group
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


DEFAULT_MEMORY = "# MEMORY\n\nUser preferences, facts, and personalization notes are recorded here.\n"
DEFAULT_MEMORY_HOST = "# HOST MEMORY\n\nHost-specific settings, hardware, local tools, and repo paths.\n"
DEFAULT_MEMORY_TMP = (
    "# PENDING MEMORY (staging)\n\n"
    "Candidates appended by sessions; merged into MEMORY.md by the daily memory "
    "maintenance task.\n"
)

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

  # Seed global knowledge files
  _seed_if_missing(cfg.memory_file, DEFAULT_MEMORY)
  _seed_if_missing(cfg.memory_host_file, DEFAULT_MEMORY_HOST)
  _seed_if_missing(cfg.memory_tmp_file, DEFAULT_MEMORY_TMP)
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


def seed_default_cron_tasks(cfg: CharlieBotConfig) -> list[dict]:
  """Seed repo-owned default cron tasks into the host cron.yaml by name.

  Reads ``configs/cron.default.yaml`` from the repo and the host
  ``~/.charliebot/config.d/cron.yaml``. For each default entry: if no host entry
  shares its ``name``, append it verbatim; if one exists, change nothing about
  it. Never reorders, rewrites, or drops host-only entries.

  Creates ``config.d/`` and the cron file when absent. Writes through
  :func:`src.core.yaml_utils.save_yaml` (the same writer ``src/api/cron.py``
  uses). Validates every default entry before writing: each must construct a
  :class:`ScheduledTaskConfig` after ``prompt_file``/``local`` resolution and
  every ``prompt_file`` must resolve to an existing file. Fails loudly without
  writing if validation fails.

  Returns a per-entry report: ``[{"name": str, "status": "created"|"exists"}, ...]``.

  This is a library function invoked only by ``./scripts/setup.sh``. It is NOT
  called by :func:`init_charliebot_home`, so the server startup path never
  writes ``cron.yaml`` — that is an invariant the tests assert directly.
  """
  repo_root = cfg.charlie_bot_repo
  defaults_data = load_yaml(repo_root / "configs" / "cron.default.yaml", default={})
  default_entries = list(defaults_data.get("scheduled_tasks", []))

  # Validate every default entry on a resolved copy before touching the host
  # file, so a bad repo default fails loudly without any partial write.
  for entry in default_entries:
    if not isinstance(entry, dict):
      raise ValueError(f"invalid default cron entry (not a mapping): {entry!r}")
    resolved = copy.deepcopy(entry)
    _resolve_prompt_file(resolved, repo_root)  # raises ScheduledTaskResolutionError
    _resolve_local_timezone(resolved)
    ScheduledTaskConfig(**resolved)  # raises on validation error

  cfg.config_d_dir.mkdir(parents=True, exist_ok=True)
  cron_path = cfg.config_d_dir / "cron.yaml"
  data = load_yaml(cron_path, default={"scheduled_tasks": []})
  if not isinstance(data, dict):
    raise ValueError(f"cron config must be a mapping: {cron_path}")
  host_tasks = list(data.get("scheduled_tasks", []) or [])
  host_names = {t.get("name") for t in host_tasks if isinstance(t, dict)}

  report: list[dict] = []
  changed = False
  for entry in default_entries:
    name = entry.get("name")
    if name in host_names:
      report.append({"name": name, "status": "exists"})
      continue
    host_tasks.append(copy.deepcopy(entry))
    host_names.add(name)
    report.append({"name": name, "status": "created"})
    changed = True

  if changed:
    data["scheduled_tasks"] = host_tasks
    save_yaml(cron_path, data)
  return report


async def run_crash_recovery(cfg, boot_time: datetime) -> int:
  """Recover orphaned threads, quarantine stale worktrees, and clear stale thinking.

  This is the deferrable part of startup: the server lifespan launches it as a
  background task so readiness is not delayed by the O(history) per-thread
  metadata scan. The two synchronous scans run via ``asyncio.to_thread`` so they
  never block the event loop while live requests are served; the async git
  quarantine calls are awaited normally.

  *boot_time* is captured at lifespan start. Only threads started before it are
  treated as orphaned, so a worker spawned during the recovery window is spared.

  Returns the number of orphaned threads recovered.
  """
  recovered, threads = await asyncio.to_thread(_recover_orphaned_threads, cfg, boot_time)
  await _quarantine_stale_failed_worktrees(cfg, threads)
  await asyncio.to_thread(_clear_stale_thinking, cfg)
  return recovered


def _recover_orphaned_threads(cfg, boot_time: datetime) -> tuple[int, list[dict]]:
  """Mark pre-boot 'running' threads as 'failed' and collect in-window thread metadata.

  On server startup, no thread should be running — they are always spawned by
  the spawner. A 'running' thread whose start predates *boot_time* is orphaned
  from a previous server lifecycle (crash, reload, or restart); kill any
  lingering worker process and mark it failed. The boot_time guard spares a
  worker spawned during the recovery window (its started_at is always post-boot).

  Only threads whose ``metadata.json`` mtime falls within ``RUNNING_SCAN_WINDOW``
  are read+parsed (via ``iter_recent_thread_metas``); older thread dirs are skipped
  entirely — neither read nor appended to the returned list. This is sound because a
  live thread's metadata is always recent, and the 30-day read window fully covers
  the 7-day quarantine band consumed by ``_quarantine_stale_failed_worktrees`` (a
  failed thread's metadata mtime equals its ``completed_at`` write time). A worktree
  from a thread completed more than the window ago and still on disk is only possible
  if the server never restarted for > window days — accepted and negligible; no extra
  machinery handles it.

  Crash-orphaned worktrees are kept (not deleted) so their in-progress state is
  available for debugging. Returns the recovered count plus the in-window thread
  metadata dicts, which feed the quarantine sweep.
  """
  if not cfg.sessions_dir.exists():
    return 0, []
  recovered = 0
  threads: list[dict] = []
  now = utc_now()
  for session_dir in cfg.sessions_dir.iterdir():
    threads_dir = session_dir / "threads"
    for thread_dir, meta_path, meta in iter_recent_thread_metas(threads_dir, now, "thread_meta_unreadable"):
      if meta.get("status") == "running" and _started_before_boot(meta, thread_dir, boot_time):
        # Kill orphaned worker process if still alive, then mark failed.
        pid = meta.get("pid")
        if pid:
          kill_process_group(pid, signal.SIGTERM)
        meta["status"] = "failed"
        meta["exit_code"] = -1
        meta["completed_at"] = utc_now().isoformat()
        meta_path.write_text(json.dumps(meta, indent=2), encoding="utf-8")
        log.warning("recovered_orphaned_thread", thread=meta.get("id"), pid=pid)
        recovered += 1
      threads.append(meta)

  if recovered:
    log.info("orphaned_thread_recovery_done", count=recovered)
  return recovered, threads


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


def _clear_stale_thinking(cfg) -> None:
  """Set thinking_since=null on all session metadata files.

  On server startup no master task is running, so any non-null thinking_since
  is stale from a previous crash or unclean shutdown.
  """
  if not cfg.sessions_dir.exists():
    return
  cleared = 0
  for session_dir in cfg.sessions_dir.iterdir():
    meta_path = session_dir / "metadata.json"
    meta = load_json_meta(meta_path, "session_meta_unreadable")
    if meta is None:
      continue
    if meta.get("thinking_since") is None:
      continue
    meta["thinking_since"] = None
    meta_path.write_text(json.dumps(meta, indent=2), encoding="utf-8")
    log.debug("cleared_stale_thinking_since", session=meta.get("id"))
    cleared += 1

  if cleared:
    log.info("stale_thinking_since_cleared", count=cleared)
