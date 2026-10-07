"""CLI script to stop the session's running improve loop.

Called by the master Claude Code instance as a shell command (session
identity resolves per ``resolve_session_id``):

  charliebot improve-stop

The CLI posts to the server-side /api/internal/improve/stop endpoint, which
marks the loop stopped; the current iteration finishes and no further
iteration starts, so the next `charliebot improve` in this session launches a
new loop. Exit code 0 follows a stop; exit code 1 reports a session with no
running loop.
"""

import argparse

from src.infra import help_formatter
from src.runtime.cli import common


def main() -> None:
  parser = argparse.ArgumentParser(
      description="Stop this session's active improve loop after the current iteration",
      formatter_class=help_formatter.CliRawDescriptionHelpFormatter,
  )
  common.add_session_arg(parser)
  args = parser.parse_args()
  session_id = common.resolve_session_id(args.session)
  common.post_internal_api("/api/internal/improve/stop", {"session_id": session_id})
  print("stopped")


if __name__ == "__main__":
  main()
