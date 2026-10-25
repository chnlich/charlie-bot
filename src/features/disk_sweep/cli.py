"""Command line entry point for listing or applying the disk sweep."""

import asyncio
import sys

from src.features.disk_sweep import sweep
from src.infra import config, help_formatter


def main() -> None:
  parser = help_formatter.CliArgumentParser(description="List or remove old task worktrees and scratch directories")
  parser.add_argument(
      "--yes",
      action="store_true",
      help="Archive changed worktrees and remove eligible paths. Without it, this is a read-only dry-run.")
  args = parser.parse_args()

  report = asyncio.run(sweep.run_sweep(config.get_config(), apply=args.yes))
  for row in report.rows:
    print(row.to_tsv())
  print(report.total_line())
  for failure in report.failures:
    print(f"{failure.path}: {failure.reason}", file=sys.stderr)
  if report.failures:
    sys.exit(1)


if __name__ == "__main__":
  main()
