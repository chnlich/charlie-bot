"""Unified CharlieBot CLI entrypoint.

Canonical invocations:

  charliebot delegate ...
  charliebot improve ...
  charliebot schedule-trigger ...
  charliebot remote-launch ...

The legacy ``python -m src.cli.<command>`` entrypoints remain owned by their
individual modules.
"""

import importlib
import sys
from collections.abc import Sequence
from pathlib import Path

_COMMANDS = {
    "delegate": "src.cli.delegate",
    "improve": "src.cli.improve",
    "schedule-trigger": "src.cli.schedule_trigger",
    "remote-launch": "src.cli.remote_launch",
    "gc-trash": "src.cli.gc_trash",
}


def _print_help(prog: str) -> None:
  print(f"usage: {prog} <subcommand> [args...]")
  print()
  print("Available subcommands:")
  for subcommand in sorted(_COMMANDS):
    print(f"  {subcommand}")


def main(argv: Sequence[str] | None = None) -> None:
  """Dispatch to a subcommand's existing main() without duplicating its parser."""
  prog = Path(sys.argv[0]).name if argv is None and sys.argv else "charliebot"
  args = list(sys.argv[1:] if argv is None else argv)

  if not args or args[0] in {"-h", "--help"}:
    _print_help(prog)
    return

  subcommand = args[0]
  module_name = _COMMANDS.get(subcommand)
  if module_name is None:
    available = ", ".join(sorted(_COMMANDS))
    print(f"{prog}: unknown subcommand {subcommand!r}. Available subcommands: {available}", file=sys.stderr)
    sys.exit(2)

  module = importlib.import_module(module_name)
  original_argv = sys.argv
  sys.argv = [f"{prog} {subcommand}", *args[1:]]
  try:
    module.main()
  finally:
    sys.argv = original_argv


if __name__ == "__main__":
  main()
