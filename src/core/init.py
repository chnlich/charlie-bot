"""Initialize ~/.charliebot/ directory structure on first run."""

import json
import signal
from datetime import timedelta

import yaml
from pathlib import Path

import structlog

from src.core.config import get_config
from src.core.git import git_quarantine_worktree, git_worktree_dir_name
from src.core.json_utils import load_json_meta
from src.core.models import parse_utc_datetime, utc_now
from src.core.process import kill_process_group
from src.core.worktree_trash import dir_size_bytes, format_size, trash_dir

log = structlog.get_logger()

# Failed worktrees older than this are swept into <worktree_dir>/.trash/ on startup.
# Kept (not hard-deleted) so recent failures stay available for debugging.
FAILED_WORKTREE_QUARANTINE_DAYS = 7

# Thread statuses that mean the worker is done; anything else still owns its worktree.
_TERMINAL_THREAD_STATUSES = frozenset({"completed", "failed", "cancelled"})


def _default_config_yaml() -> dict:
  """Build the default config dict with placeholder values."""
  return {
      "gemini_api_key": "",
      "gemini_model": "gemini-3.1-pro-preview",
      "workspace_dirs": ["~/workspace"],
      "worktree_dir": "~/worktrees",
  }


DEFAULT_MEMORY = "# MEMORY\n\nUser preferences, facts, and personalization notes are recorded here.\n"
DEFAULT_MEMORY_HOST = "# HOST MEMORY\n\nHost-specific settings, hardware, local tools, and repo paths.\n"

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
  _seed_if_missing(cfg.charliebot_home / 'slash_commands.yaml', DEFAULT_SLASH_COMMANDS)

  # Seed config.yaml with placeholders if missing
  if not cfg.config_file.exists():
    with open(cfg.config_file, "w") as f:
      yaml.dump(_default_config_yaml(), f, default_flow_style=False, sort_keys=False)

  # Recover orphaned threads from previous server crash/restart and sweep stale
  # failed worktrees into the quarantine trash.
  await _recover_orphaned_threads(cfg)
  # Clear stale thinking_since from sessions left over from a crash
  _clear_stale_thinking(cfg)


def _seed_if_missing(path: Path, content: str) -> None:
  """Write content to path only if the file does not already exist."""
  if not path.exists():
    path.write_text(content, encoding="utf-8")


async def _recover_orphaned_threads(cfg) -> None:
  """Mark threads stuck in 'running' as 'failed', then quarantine stale failed worktrees.

  On server startup, no thread should be running — they are always spawned
  by the spawner. Any 'running' thread is orphaned from a previous server
  lifecycle (crash, reload, or restart). Kill any lingering worker processes.

  Crash-orphaned worktrees are kept (not deleted) so their in-progress state is
  available for debugging. The same walk feeds a quarantine sweep that, once a
  failed thread's worktree has aged past FAILED_WORKTREE_QUARANTINE_DAYS, moves it
  into <worktree_dir>/.trash/ to bound disk growth without hard-deleting anything.
  """
  if not cfg.sessions_dir.exists():
    return
  recovered = 0
  threads: list[dict] = []
  for session_dir in cfg.sessions_dir.iterdir():
    threads_dir = session_dir / "threads"
    if not threads_dir.is_dir():
      continue
    for thread_dir in threads_dir.iterdir():
      meta_path = thread_dir / "metadata.json"
      meta = load_json_meta(meta_path, "thread_meta_unreadable")
      if meta is None:
        continue
      if meta.get("status") == "running":
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

  await _quarantine_stale_failed_worktrees(cfg, threads)


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

  if trash_path.exists():
    total = dir_size_bytes(trash_path)
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
