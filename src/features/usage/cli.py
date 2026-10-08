"""CLI: fill the SQLite usage ledger by hand.

Usage:
  charliebot usage-ledger capture [--ledger PATH]

The capture itself lives in src.features.usage.token_tally (capture_local), the same entry point
the collector and the cold-storage sweep call, so the CLI cannot drift from it.
"""

import argparse
import pathlib

from src.infra import help_formatter


def _ledger_path(args: argparse.Namespace) -> pathlib.Path:
  if args.ledger is not None:
    return args.ledger
  # Deferred like the other core stacks: --help and parser errors stay off the config import.
  from src.features.usage import usage_ledger

  return usage_ledger.default_ledger_path()


def _print_written(written: dict[str, int]) -> None:
  for source in sorted(written):
    print(f"{source}: {written[source]} records written")


def _cmd_capture(args: argparse.Namespace) -> None:
  from src.features.usage import token_tally, usage_ledger

  with usage_ledger.UsageLedger(_ledger_path(args)) as ledger:
    _print_written(token_tally.capture_local(ledger))


def main() -> None:
  parser = argparse.ArgumentParser(
      description="CharlieBot usage ledger", formatter_class=help_formatter.CliHelpFormatter)
  sub = parser.add_subparsers(dest="command", required=True)

  capture = sub.add_parser(
      "capture",
      help="Capture this host's own CLI logs into the ledger",
      formatter_class=help_formatter.CliHelpFormatter,
      description="Capture this host's own CLI logs into the ledger with their host defaults: every registered "
      "usage source's logs and the charlie-bot session tree.")
  capture.add_argument(
      "--ledger", type=pathlib.Path, help="SQLite ledger path (default: <charliebot home>/usage/ledger.sqlite3).")

  args = parser.parse_args()
  _cmd_capture(args)


if __name__ == "__main__":
  main()
