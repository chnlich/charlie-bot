"""CLI script for the ``charliebot artifact`` subcommand.

  charliebot artifact check <file> --genre plan|understanding|sitrep|debug|explain
      [--trigger "<message>"] [--assertions-only]
  charliebot artifact wrap <fragment> --genre <genre> --output <page.html> [--math/--no-math]

Local only: no session resolution, no HTTP, no registry write. ``check`` runs the genre's
mechanical DOM assertions, prints one line per assertion (``ok <name>[ <measurement>]`` or
``FAIL <name>: <location>``), and — for every genre, once every assertion passed — runs
the cold-read probe unless ``--assertions-only`` was given; ``--trigger`` is required for
every genre unless ``--assertions-only``. ``wrap`` assembles a genre page from a content
fragment (the fragment fills <body>; head and style come from the genre template), pre-rendering
math to KaTeX markup for explain by default. Exit codes: 0 = success, 1 = any assertion
failed, the probe could not run, or the assembled page failed the byte self-check, 2 = usage
error.
"""

import argparse
import sys
from collections.abc import Sequence
from pathlib import Path

from src.cli import common as cli_common
from src.core import artifact_check, artifact_wrap
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
  wrap = sub.add_parser("wrap", help="Assemble a genre page from a content fragment")
  wrap.add_argument("fragment", help="Content fragment path: the page's <body> content")
  wrap.add_argument(
      "--genre", required=True, choices=artifact_check.GENRES, help="Genre whose template shells the page")
  wrap.add_argument("--output", required=True, help="Assembled page path (the artifacts path to write)")
  wrap.add_argument(
      "--math",
      action=argparse.BooleanOptionalAction,
      default=None,
      help="Pre-render math to KaTeX markup at assembly time (default: on for explain, off for other genres)")
  return parser


def _run_check(args: argparse.Namespace) -> int:
  if args.trigger is None and not args.assertions_only:
    cli_common.exit_usage_error(f"--genre {args.genre} requires --trigger unless --assertions-only is given")
  artifact = Path(args.file).resolve()
  if not artifact.is_file():
    cli_common.exit_error(f"artifact not found: {args.file}")
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
    cli_common.exit_error(str(e))
  for backend_id, error in result.attempts:
    print(f"attempt {backend_id} failed: {error}")
  if result.backend_id is None:
    print(f"probe could not run: every backend failed ({len(result.attempts)} tried)")
    return 1
  print(f"backend {result.backend_id}")
  print(result.answer)
  return 0


def _run_wrap(args: argparse.Namespace) -> int:
  fragment = Path(args.fragment).resolve()
  if not fragment.is_file():
    cli_common.exit_usage_error(f"fragment not found: {args.fragment}")
  math = args.math if args.math is not None else args.genre == "explain"
  try:
    written = artifact_wrap.wrap_fragment(
        genre=args.genre,
        fragment=fragment,
        output=Path(args.output).resolve(),
        math=math,
        vendor_path=artifact_wrap.vendor_katex_path(get_config().charliebot_home),
    )
  except (RuntimeError, ValueError) as e:
    cli_common.exit_error(str(e))
  print(f"wrote {written}")
  return 0


def main(argv: Sequence[str] | None = None) -> None:
  parser = _build_parser()
  args = parser.parse_args(argv if argv is not None else None)
  if args.verb == "check":
    sys.exit(_run_check(args))
  if args.verb == "wrap":
    sys.exit(_run_wrap(args))


if __name__ == "__main__":
  main()
