"""Unified CharlieBot CLI entrypoint.

The subcommand vocabulary has one definition: the ``_COMMANDS`` registry
below, which ``--help`` prints and whose full set the README's "CLI at a
glance" section must name. The legacy ``python -m <module>``
entrypoints remain owned by their individual modules.
"""

import importlib
import os.path
import sys
from collections.abc import Sequence

_COMMANDS = {
    "artifact": "src.features.artifacts.cli",
    "config": "src.runtime.cli.config",
    "delegate": "src.runtime.cli.delegate",
    "discord": "src.features.discord.cli",
    "improve": "src.features.improve.cli",
    "improve-stop": "src.features.improve.stop_cli",
    "schedule-trigger": "src.runtime.cli.schedule_trigger",
    "remote-launch": "src.features.remote_launch.cli",
    "gc-trash": "src.runtime.cli.gc_trash",
    "plan": "src.features.artifacts.plan_cli",
    "publish": "src.features.artifacts.publish_cli",
    "memory": "src.features.memory.cli",
    "session": "src.runtime.cli.session",
    "session-tree": "src.features.session_tree_preview.cli",
    "slack": "src.features.slack.cli",
    "storage": "src.features.storage.cli",
    "usage-ledger": "src.features.usage.cli",
}


def _print_help(prog: str) -> None:
  print(f"usage: {prog} <subcommand> [args...]")
  print()
  print("Available subcommands:")
  for subcommand in sorted(_COMMANDS):
    print(f"  {subcommand}")


def main(argv: Sequence[str] | None = None) -> None:
  """Dispatch to a subcommand's existing main() without duplicating its parser."""
  prog = os.path.basename(sys.argv[0]) if argv is None and sys.argv else "charliebot"
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
