"""Helpers for the worktree quarantine trash dir (`<worktree_dir>/.trash/`).

Stale failed worktrees are *moved* here by the startup sweep (never hard-deleted),
and only the manual `charliebot gc-trash --yes` command ever removes them. These
helpers are shared by the sweep (size reporting) and the CLI (listing + purge).
"""

import os
from dataclasses import dataclass
from pathlib import Path

import structlog

from src.core.models import utc_now

log = structlog.get_logger()

_TRASH_DIR_NAME = ".trash"


def trash_dir(worktree_dir: str) -> Path:
  """Return the quarantine trash dir under the worktree root."""
  return Path(worktree_dir) / _TRASH_DIR_NAME


def dir_size_bytes(path: Path) -> int:
  """Total size in bytes of all files under path; unreadable entries are skipped."""
  total = 0
  for dirpath, _, filenames in os.walk(path, followlinks=False):
    for name in filenames:
      file_path = Path(dirpath) / name
      try:
        total += file_path.lstat().st_size
      except OSError as e:
        log.warning("trash_size_stat_failed", path=str(file_path), error=str(e))
  return total


def format_size(num_bytes: int) -> str:
  """Format a byte count as a human-readable string."""
  size = float(num_bytes)
  for unit in ("B", "KB", "MB", "GB", "TB"):
    if size < 1024 or unit == "TB":
      return f"{size:.1f} {unit}"
    size /= 1024
  return f"{size:.1f} TB"


@dataclass(frozen=True)
class TrashEntry:
  """A single top-level directory sitting in the quarantine trash."""
  path: Path
  age_days: float
  size_bytes: int


def list_trash_entries(trash_path: Path) -> list[TrashEntry]:
  """List top-level entries in the trash dir with their age (since last modified) and size."""
  if not trash_path.is_dir():
    return []
  now = utc_now().timestamp()
  entries: list[TrashEntry] = []
  for child in sorted(trash_path.iterdir()):
    age_days = max(0.0, (now - child.lstat().st_mtime) / 86400.0)
    entries.append(TrashEntry(path=child, age_days=age_days, size_bytes=dir_size_bytes(child)))
  return entries
