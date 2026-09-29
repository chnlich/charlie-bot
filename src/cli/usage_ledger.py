"""CLI: fill the SQLite usage ledger by hand.

Usage:
  charliebot usage-ledger capture [--ledger PATH]
  charliebot usage-ledger import --host NAME [--ledger PATH]
      [--claude-home LABEL=DIR]... [--codex-home LABEL=DIR]... [--opencode-db PATH]

The capture itself lives in src.core.token_tally (capture_local / capture_usage), the same
entry points the collector and the page's sweep gate call, so the CLI cannot drift from them.
``import`` is the recipe the go-live check runs to bring the old host's history in: point each
flag at the copied home on this host, labeled with its name there.
"""

import argparse
from pathlib import Path

from src.cli.help_formatter import CliHelpFormatter


def _home_pair(value: str) -> tuple[str, Path]:
  label, sep, path = value.partition("=")
  if not sep or not label or not path:
    raise argparse.ArgumentTypeError(f"expected LABEL=DIR, got {value!r}")
  home = Path(path)
  if not home.is_dir():
    raise argparse.ArgumentTypeError(f"directory does not exist: {home}")
  return label, home


def _ledger_path(args: argparse.Namespace) -> Path:
  if args.ledger is not None:
    return args.ledger
  # Deferred like the other core stacks: --help and parser errors stay off the config import.
  from src.core.usage_ledger import default_ledger_path

  return default_ledger_path()


def _print_written(written: dict[str, int]) -> None:
  for source in sorted(written):
    print(f"{source}: {written[source]} records written")


def _cmd_capture(args: argparse.Namespace) -> None:
  from src.core.token_tally import capture_local
  from src.core.usage_ledger import UsageLedger

  with UsageLedger(_ledger_path(args)) as ledger:
    _print_written(capture_local(ledger))


def _cmd_import(args: argparse.Namespace) -> None:
  from src.core.token_tally import capture_usage
  from src.core.usage_ledger import UsageLedger

  with UsageLedger(_ledger_path(args)) as ledger:
    written = capture_usage(
        ledger,
        host=args.host,
        claude_homes=dict(args.claude_home),
        codex_homes=dict(args.codex_home),
        opencode_db=args.opencode_db,
        sessions_dir=None,
        cache_path=None,
    )
  _print_written(written)


def main() -> None:
  parser = argparse.ArgumentParser(description="CharlieBot usage ledger", formatter_class=CliHelpFormatter)
  sub = parser.add_subparsers(dest="command", required=True)

  ledger_help = "SQLite ledger path (default: <charliebot home>/usage/ledger.sqlite3)."

  capture = sub.add_parser(
      "capture",
      help="Capture this host's own CLI logs into the ledger",
      formatter_class=CliHelpFormatter,
      description="Capture this host's own CLI logs into the ledger with their host defaults: the discovered "
      "Claude config dirs and Codex homes, the opencode db, the charlie-bot session tree.")
  capture.add_argument("--ledger", type=Path, help=ledger_help)

  imp = sub.add_parser(
      "import",
      help="Backfill another host's copied logs into the ledger",
      formatter_class=CliHelpFormatter,
      description="Backfill another host's history from its copied logs: each home flag names the copied "
      "directory on this host, labeled with its name on the host it came from.")
  imp.add_argument("--host", required=True, help="Host the copied logs came from.")
  imp.add_argument("--ledger", type=Path, help=ledger_help)
  imp.add_argument(
      "--claude-home",
      action="append",
      type=_home_pair,
      default=[],
      metavar="LABEL=DIR",
      help="A copied Claude Code config dir; repeatable.")
  imp.add_argument(
      "--codex-home",
      action="append",
      type=_home_pair,
      default=[],
      metavar="LABEL=DIR",
      help="A copied Codex home; repeatable.")
  imp.add_argument("--opencode-db", type=Path, help="A copied opencode sqlite db.")

  args = parser.parse_args()
  if args.command == "import" and not (args.claude_home or args.codex_home or args.opencode_db):
    parser.error("import needs at least one source flag: --claude-home, --codex-home or --opencode-db")
  {"capture": _cmd_capture, "import": _cmd_import}[args.command](args)


if __name__ == "__main__":
  main()
