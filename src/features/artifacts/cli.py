"""CLI script for the ``charliebot artifact`` subcommand.

  charliebot artifact check <file> --genre plan|understanding|sitrep|debug|explain
      [--trigger "<message>"] [--assertions-only]
  charliebot artifact wrap <fragment> --genre <genre> --output <page.html> [--math/--no-math]

``check`` runs the genre's mechanical DOM assertions, prints one line per assertion
(``ok <name>[ <measurement>]`` or ``FAIL <name>: <location>``), and — for every genre,
once every assertion passed — runs the cold read unless ``--assertions-only`` was given;
``--trigger`` is required for every genre unless ``--assertions-only``. The cold read's
shape follows the session the command runs in, read from the session metadata's profile:
in a manager session the command POSTs a cold-read delegation to /api/internal/delegate,
prints ``cold read started: session <child id> run <run id>``, and exits 0 — the cold read
runs as a cold-read worker child of the session, and its completion report wakes the
session; in a worker session, or with no session identity (CHARLIEBOT_SESSION_ID absent
and the cwd not a session directory), the command runs the probe in the foreground and
prints the report body — the page path, the page bytes' sha256, one line per failed
attempt, the answering backend and the seven answers verbatim — the same body the child
session's report carries. A failed assertion or a refused delegation prints the reason
and exits 1. ``wrap`` assembles a genre page from a content fragment (the fragment fills
<body>; head and style come from the genre template), pre-rendering math to KaTeX markup
for explain by default. Exit codes: 0 = success, 1 = any assertion failed, the probe
could not run, or the assembled page failed the byte self-check, 2 = usage error.
"""

from __future__ import annotations

import argparse
import datetime
import hashlib
import json
import os
import pathlib
import sys
from collections.abc import Sequence
from typing import TYPE_CHECKING

from src.features.artifacts import artifact_check, artifact_wrap, constants
from src.infra import constants as infra_constants
from src.infra import help_formatter, home
from src.runtime import cold_read
from src.runtime.cli import common as cli_common

if TYPE_CHECKING:
  from src.infra import config


def _build_parser() -> argparse.ArgumentParser:
  parser = argparse.ArgumentParser(
      prog="charliebot artifact", description="Artifact checks", formatter_class=help_formatter.CliHelpFormatter)
  sub = parser.add_subparsers(dest="verb", required=True)
  check = sub.add_parser(
      "check",
      help="Run a genre's assertions (and cold read) on a local file",
      formatter_class=help_formatter.CliHelpFormatter)
  check.add_argument("file", help="Artifact path as an ordinary filesystem path (absolute or cwd-relative)")
  check.add_argument(
      "--genre", required=True, choices=constants.ARTIFACT_GENRES, help="Genre the page claims to follow")
  check.add_argument(
      "--trigger",
      default=None,
      help="Chat message that triggered the page (question 6 verbatim); required for every genre "
      "unless --assertions-only is given")
  check.add_argument("--assertions-only", action="store_true", help="Run the assertions alone, skipping the cold read")
  wrap = sub.add_parser(
      "wrap", help="Assemble a genre page from a content fragment", formatter_class=help_formatter.CliHelpFormatter)
  wrap.add_argument("fragment", help="Content fragment path: the page's <body> content")
  wrap.add_argument(
      "--genre", required=True, choices=constants.ARTIFACT_GENRES, help="Genre whose template shells the page")
  wrap.add_argument("--output", required=True, help="Assembled page path (the artifacts path to write)")
  wrap.add_argument(
      "--math",
      action=argparse.BooleanOptionalAction,
      default=None,
      help="Pre-render math to KaTeX markup at assembly time (default: on for explain, off for other genres)")
  return parser


def _session_identity(cfg: config.CharlieBotConfig) -> str | None:
  """This invocation's session id, or None with no session identity.

  No identity means ``CHARLIEBOT_SESSION_ID`` is absent and the cwd is not a
  session directory — the contract ``cli_common.resolve_session_id`` summarizes.
  """
  env_session = os.environ.get(infra_constants.SESSION_ID_ENV_VAR) or None
  if env_session is not None:
    return env_session
  cwd = pathlib.Path.cwd().resolve()
  sessions_dir = cfg.sessions_dir.resolve()
  return cwd.name if cwd.parent == sessions_dir else None


def _session_profile(cfg: config.CharlieBotConfig, session_id: str) -> str:
  """The requesting session's profile (``manager`` or ``worker``) from its session metadata."""
  meta_path = cfg.sessions_dir / session_id / "metadata.json"
  try:
    profile = json.loads(meta_path.read_text(encoding="utf-8"))["profile"]
  except (OSError, ValueError, KeyError) as e:
    cli_common.exit_error(f"cannot read the requesting session's profile from {meta_path}: {e}")
  return profile


def _delegate_cold_read(args: argparse.Namespace, artifact: pathlib.Path, session_id: str) -> int:
  """Delegate the cold read to a cold-read child of this manager session and print the started line.

  The request id binds the session, a sha256 prefix of the page, and the UTC
  time, so a replayed call returns the original child and a changed page a new one.
  """
  digest = hashlib.sha256(artifact.read_bytes()).hexdigest()
  timestamp = datetime.datetime.now(datetime.UTC).strftime("%Y%m%dT%H%M%SZ")
  request_id = f"cold-read:{session_id}:{digest[:12]}:{timestamp}"
  description = f"cold read of {artifact}"
  payload = {
      "session_id": session_id,
      "description": description,
      "task_type": "cold-read",
      "cold_read": {
          "page_path": str(artifact),
          "genre": args.genre,
          "trigger": args.trigger
      },
      "request_id": request_id,
  }
  result = cli_common.post_internal_api(
      "/api/internal/delegate",
      payload,
      readback=lambda: cli_common.find_local_task_child(
          session_id, description=description, task_type="cold-read", request_id=request_id))
  print(f"cold read started: session {result['session_id']} run {result['run_id']}")
  return 0


def _run_check(args: argparse.Namespace) -> int:
  if args.trigger is None and not args.assertions_only:
    cli_common.exit_usage_error(f"--genre {args.genre} requires --trigger unless --assertions-only is given")
  artifact = pathlib.Path(args.file).resolve()
  if not artifact.is_file():
    cli_common.exit_error(f"artifact not found: {args.file}")
  cfg = cli_common.get_config()
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
  session_id = _session_identity(cfg)
  if session_id is not None and _session_profile(cfg, session_id) == "manager":
    return _delegate_cold_read(args, artifact, session_id)
  # A worker session, or no session identity: the cold read runs in the foreground.
  try:
    result = artifact_check.run_probe(cfg, artifact, args.trigger)
  except ValueError as e:
    cli_common.exit_error(str(e))
  print(cold_read.format_report(str(artifact), result.page_sha256, result.attempts, result.backend_id, result.answer))
  return 0 if result.backend_id is not None else 1


def _run_wrap(args: argparse.Namespace) -> int:
  fragment = pathlib.Path(args.fragment).resolve()
  if not fragment.is_file():
    cli_common.exit_usage_error(f"fragment not found: {args.fragment}")
  math = args.math if args.math is not None else args.genre == "explain"
  try:
    written = artifact_wrap.wrap_fragment(
        genre=args.genre,
        fragment=fragment,
        output=pathlib.Path(args.output).resolve(),
        math=math,
        vendor_path=artifact_wrap.vendor_katex_path(home.charliebot_home_dir()),
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
