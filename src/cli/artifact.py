"""CLI script for the ``charliebot artifact`` subcommand.

  charliebot artifact check <file> --genre plan|understanding|sitrep|debug|explain
      [--trigger "<message>"] [--assertions-only]

Local only: no session resolution, no HTTP, no registry write. ``check`` runs the genre's
mechanical DOM assertions, prints one line per assertion (``ok <name>[ <measurement>]`` or
``FAIL <name>: <location>``), and — for every genre, once every assertion passed — runs
the cold-read probe unless ``--assertions-only`` was given; ``--trigger`` is required for
every genre unless ``--assertions-only``. Exit codes: 0 = every assertion passed (probe
answers print verbatim and are never judged), 1 = any assertion failed or the probe could
not run, 2 = usage error.
"""

import argparse
import json
import sys
from collections.abc import Sequence
from pathlib import Path

from src.cli.common import exit_usage_error
from src.core import artifact_check
from src.core.config import get_config


def _build_parser() -> argparse.ArgumentParser:
  parser = argparse.ArgumentParser(prog="charliebot artifact", description="Artifact checks (local files only)")
  sub = parser.add_subparsers(dest="verb", required=True)
  check = sub.add_parser("check", help="Run a genre's assertions (and cold-read probe) on a local file")
  check.add_argument("file", help="Artifact path as an ordinary filesystem path (absolute or cwd-relative)")
  check.add_argument("--genre", required=True, choices=artifact_check.GENRES, help="Genre the page claims to follow")
  check.add_argument(
      "--trigger",
      default=None,
      help="Chat message that triggered the page (question 6 verbatim); required for every genre "
      "unless --assertions-only is given")
  check.add_argument(
      "--assertions-only", action="store_true", help="Run the assertions alone, skipping the cold-read probe")
  return parser


def _run_check(args: argparse.Namespace) -> int:
  if args.trigger is None and not args.assertions_only:
    exit_usage_error(f"--genre {args.genre} requires --trigger unless --assertions-only is given")
  artifact = Path(args.file).resolve()
  if not artifact.is_file():
    print(json.dumps({"error": f"artifact not found: {args.file}"}), file=sys.stderr)
    return 1
  cfg = get_config()
  failed = 0
  for outcome in artifact_check.run_assertions(args.genre, artifact, cfg):
    if outcome.passed:
      print(f"ok {outcome.name}" + (f" {outcome.detail}" if outcome.detail else ""))
    else:
      failed += 1
      print(f"FAIL {outcome.name}: {outcome.detail}")
  if failed:
    return 1
  if args.assertions_only:
    return 0
  print("--- cold read ---")
  try:
    result = artifact_check.run_probe(cfg, artifact, args.trigger)
  except ValueError as e:
    print(json.dumps({"error": str(e)}), file=sys.stderr)
    return 1
  for backend_id, error in result.attempts:
    print(f"attempt {backend_id} failed: {error}")
  if result.backend_id is None:
    print(f"probe could not run: every backend failed ({len(result.attempts)} tried)")
    return 1
  print(f"backend {result.backend_id}")
  print(result.answer)
  return 0


def main(argv: Sequence[str] | None = None) -> None:
  parser = _build_parser()
  args = parser.parse_args(argv if argv is not None else None)
  if args.verb == "check":
    sys.exit(_run_check(args))


if __name__ == "__main__":
  main()
