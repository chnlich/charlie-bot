"""Unified CharlieBot CLI entrypoint.

The subcommand vocabulary has two definitions: the ``_RUNTIME_COMMANDS`` table
below, and the commands each package registers through
``src.runtime.hooks.wiring`` (the package list is ``src.app.registrations``).
``--help`` prints their union, and the README's "CLI at a glance" section must
name the full set. The legacy ``python -m <module>`` entrypoints remain owned by
their individual modules.
"""

import importlib
import os.path
import sys
from collections.abc import Sequence

from src.app import registrations
from src.runtime.hooks import wiring

_RUNTIME_COMMANDS = {
    "config": "src.runtime.cli.config",
    "delegate": "src.runtime.cli.delegate",
    "schedule-trigger": "src.runtime.cli.schedule_trigger",
    "gc-trash": "src.runtime.cli.gc_trash",
    "session": "src.runtime.cli.session",
}


def _print_help(prog: str, commands: dict[str, str]) -> None:
  print(f"usage: {prog} <subcommand> [args...]")
  print()
  print("Available subcommands:")
  for subcommand in sorted(commands):
    print(f"  {subcommand}")


def main(argv: Sequence[str] | None = None) -> None:
  """Dispatch to a subcommand's existing main() without duplicating its parser."""
  prog = os.path.basename(sys.argv[0]) if argv is None and sys.argv else "charliebot"
  args = list(sys.argv[1:] if argv is None else argv)
  registrations.register_all()
  commands = {**_RUNTIME_COMMANDS, **wiring.commands()}

  if not args or args[0] in {"-h", "--help"}:
    _print_help(prog, commands)
    return

  subcommand = args[0]
  module_name = commands.get(subcommand)
  if module_name is None:
    available = ", ".join(sorted(commands))
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
