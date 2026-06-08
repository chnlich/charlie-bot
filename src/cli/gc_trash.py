"""CLI: inspect and (only with --yes) purge the worktree quarantine trash.

The startup sweep moves stale failed worktrees into `<worktree_dir>/.trash/`; nothing
ever hard-deletes them automatically. This command lists what is there and, only when
`--yes` is passed, removes it. Root-owned entries are reported (with the manual `sudo rm`
to run) and never escalated automatically.

  charliebot gc-trash          # dry-run: list entries, delete nothing
  charliebot gc-trash --yes    # hard-delete every entry
"""

import argparse
import shutil
import sys

from src.core.config import get_config
from src.core.worktree_trash import format_size, list_trash_entries, trash_dir


def main() -> None:
  parser = argparse.ArgumentParser(description="List or purge the CharlieBot worktree quarantine trash")
  parser.add_argument(
      "--yes",
      action="store_true",
      help="Actually hard-delete every entry. Without it, this is a dry-run that deletes nothing.")
  args = parser.parse_args()

  cfg = get_config()
  trash_path = trash_dir(cfg.worktree_dir)
  entries = list_trash_entries(trash_path)

  if not entries:
    print(f"Quarantine trash is empty: {trash_path}")
    return

  total = sum(entry.size_bytes for entry in entries)
  for entry in entries:
    print(f"{entry.path}  age={entry.age_days:.1f}d  size={format_size(entry.size_bytes)}")
  print(f"\n{len(entries)} entr{'y' if len(entries) == 1 else 'ies'}, {format_size(total)} total in {trash_path}")

  if not args.yes:
    print("Dry run — nothing deleted. Re-run with --yes to hard-delete.")
    return

  failed = False
  for entry in entries:
    try:
      shutil.rmtree(entry.path)
      print(f"deleted {entry.path}")
    except PermissionError as e:
      failed = True
      print(
          f"PERMISSION DENIED: {entry.path} ({e}); contains root-owned files — "
          f"remove manually with: sudo rm -rf {entry.path}",
          file=sys.stderr)
    except OSError as e:
      failed = True
      print(f"FAILED to delete {entry.path}: {e}", file=sys.stderr)

  if failed:
    sys.exit(1)


if __name__ == "__main__":
  main()
