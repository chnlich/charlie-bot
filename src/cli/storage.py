"""CLI: reclaim storage held by cold sessions and unreferenced backend records.

Usage:
  charliebot storage cool [--dry-run] [--min-idle-days N] [--session ID]

The sweep itself lives in src.core.storage_cool; the scheduler's ``cool_storage``
handler calls the same function, so the two cannot drift.
"""

import argparse
import sys

from src.core.config import get_config
from src.core.storage_cool import MIN_IDLE_DAYS, format_sweep_table, run_cool_sweep


def _cmd_cool(args: argparse.Namespace) -> None:
  try:
    result = run_cool_sweep(
        dry_run=args.dry_run,
        min_idle_days=args.min_idle_days,
        session_id=args.session,
        cfg=get_config(),
    )
  except ValueError as e:
    print(f"Error: {e}", file=sys.stderr)
    sys.exit(1)
  print(format_sweep_table(result))


def main() -> None:
  parser = argparse.ArgumentParser(description="CharlieBot storage reclamation")
  sub = parser.add_subparsers(dest="command")
  sub.required = True

  cool = sub.add_parser(
      "cool",
      help="Delete transport logs and backend records of cold sessions",
      description="Delete the bytes no reader can reach again: cold sessions' raw "
      "transport files and the backend conversation stores the cold rule names.")
  cool.add_argument("--dry-run", action="store_true", help="Report what would be freed; write nothing, delete nothing.")
  cool.add_argument(
      "--min-idle-days",
      type=int,
      default=MIN_IDLE_DAYS,
      help=f"Idle age (days) at which an archived session counts as cold (default: {MIN_IDLE_DAYS}).")
  cool.add_argument("--session", help="Limit the whole sweep to one session; the cold rule still applies.")

  args = parser.parse_args()
  {"cool": _cmd_cool}[args.command](args)


if __name__ == "__main__":
  main()
