"""CLI: the first router program, one JSON object in on stdin and one out on stdout.

  charliebot router   <- {"role", "text", "candidates": [{"id", "cooling_until"}]}
                      -> {"backends": [...], "reason": "design p=0.86"}

The answer is the only thing printed on stdout. Every failure — a broken
``<charliebot home>/router.yaml``, a missing Jev key, malformed stdin, a Jev error, a second HTTP 429 —
writes its message on stderr and exits 1; the command never prints a fallback answer.
"""

import sys

from src.features.router import routing
from src.infra import help_formatter
from src.runtime.cli import common


def main() -> None:
  parser = help_formatter.CliArgumentParser(
      description="Choose backends for one CharlieBot task: ask Jev whether the work is routine or design work")
  parser.parse_args()
  try:
    answer = routing.route(sys.stdin.read())
  except routing.RouterError as error:
    print(f"router: {error}", file=sys.stderr)
    sys.exit(1)
  common.print_json(answer, indent=None, file=sys.stdout)


if __name__ == "__main__":
  main()
