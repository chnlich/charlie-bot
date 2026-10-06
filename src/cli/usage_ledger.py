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
import pathlib

from src.cli import help_formatter


def _home_pair(value: str) -> tuple[str, pathlib.Path]:
  label, sep, path = value.partition("=")
  if not sep or not label or not path:
    raise argparse.ArgumentTypeError(f"expected LABEL=DIR, got {value!r}")
  home = pathlib.Path(path)
  if not home.is_dir():
    raise argparse.ArgumentTypeError(f"directory does not exist: {home}")
  return label, home


def _ledger_path(args: argparse.Namespace) -> pathlib.Path:
  if args.ledger is not None:
    return args.ledger
  # Deferred like the other core stacks: --help and parser errors stay off the config import.
  from src.core import usage_ledger

  return usage_ledger.default_ledger_path()


def _print_written(written: dict[str, int]) -> None:
  for source in sorted(written):
    print(f"{source}: {written[source]} records written")


def _cmd_capture(args: argparse.Namespace) -> None:
  from src.core import token_tally, usage_ledger

  with usage_ledger.UsageLedger(_ledger_path(args)) as ledger:
    _print_written(token_tally.capture_local(ledger))


def _cmd_import(args: argparse.Namespace) -> None:
  from src.core import token_tally, usage_ledger

  with usage_ledger.UsageLedger(_ledger_path(args)) as ledger:
    written = token_tally.capture_usage(
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
  parser = argparse.ArgumentParser(
      description="CharlieBot usage ledger", formatter_class=help_formatter.CliHelpFormatter)
  sub = parser.add_subparsers(dest="command", required=True)

  ledger_help = "SQLite ledger path (default: <charliebot home>/usage/ledger.sqlite3)."

  capture = sub.add_parser(
      "capture",
      help="Capture this host's own CLI logs into the ledger",
      formatter_class=help_formatter.CliHelpFormatter,
      description="Capture this host's own CLI logs into the ledger with their host defaults: the discovered "
      "Claude config dirs and Codex homes, the opencode db, the charlie-bot session tree.")
  capture.add_argument("--ledger", type=pathlib.Path, help=ledger_help)

  imp = sub.add_parser(
      "import",
      help="Backfill another host's copied logs into the ledger",
      formatter_class=help_formatter.CliHelpFormatter,
      description="Backfill another host's history from its copied logs: each home flag names the copied "
      "directory on this host, labeled with its name on the host it came from.")
  imp.add_argument("--host", required=True, help="Host the copied logs came from.")
  imp.add_argument("--ledger", type=pathlib.Path, help=ledger_help)
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
  imp.add_argument("--opencode-db", type=pathlib.Path, help="A copied opencode sqlite db.")

  args = parser.parse_args()
  if args.command == "import" and not (args.claude_home or args.codex_home or args.opencode_db):
    parser.error("import needs at least one source flag: --claude-home, --codex-home or --opencode-db")
  {"capture": _cmd_capture, "import": _cmd_import}[args.command](args)


if __name__ == "__main__":
  main()
