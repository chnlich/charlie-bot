"""Helpers for listing and measuring the quarantined worktree trash directory.

Failed worktrees can remain under `<worktree_dir>/.trash/` for manual
inspection. The charliebot gc-trash command lists or purges those entries.
"""

import dataclasses
import os
import pathlib

from src.infra import log_once, models

log = log_once.LazyStructlogLogger()

# On-disk name of the quarantine dir under the worktree root; the storage sweep
# excludes the same directory by this name when it lists live worktrees.
TRASH_DIR_NAME = ".trash"


def trash_dir(worktree_dir: str) -> pathlib.Path:
  """Return the quarantine trash dir under the worktree root."""
  return pathlib.Path(worktree_dir) / TRASH_DIR_NAME


def dir_size_bytes(path: pathlib.Path) -> int:
  """Total size in bytes of all files under path; unreadable entries are skipped."""
  total = 0
  for dirpath, _, filenames in os.walk(path, followlinks=False):
    for name in filenames:
      file_path = pathlib.Path(dirpath) / name
      try:
        total += file_path.lstat().st_size
      except OSError as e:
        log.warning("trash_size_stat_failed", path=str(file_path), error=str(e))
  return total


@dataclasses.dataclass(frozen=True)
class TrashEntry:
  """A single top-level directory sitting in the quarantine trash."""
  path: pathlib.Path
  age_days: float
  size_bytes: int


def list_trash_entries(trash_path: pathlib.Path) -> list[TrashEntry]:
  """List top-level entries in the trash dir with their age (since last modified) and size."""
  if not trash_path.is_dir():
    return []
  now = models.utc_now().timestamp()
  entries: list[TrashEntry] = []
  for child in sorted(trash_path.iterdir()):
    age_days = max(0.0, (now - child.lstat().st_mtime) / 86400.0)
    entries.append(TrashEntry(path=child, age_days=age_days, size_bytes=dir_size_bytes(child)))
  return entries
