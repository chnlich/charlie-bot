"""CLI script for the ``charliebot plan`` subcommand.

Subcommands (verbs) follow the same caller contract as ``charliebot delegate``:
session id is resolved from cwd (explicit ``--session`` mismatches are rejected),
the result is a single JSON object on stdout, and server 4xx/5xx ``detail`` is
written to stderr as a JSON error with a non-zero exit code.

  charliebot plan present --file artifacts/plan_01.html --title "…"
  charliebot plan amend --file artifacts/plan_02.html [--plan N]
  charliebot plan approve [--plan N]
  charliebot plan close --plan N --as superseded|abandoned
  charliebot plan list
"""

import argparse
import json
import sys
from typing import Sequence

from src.cli.common import get_api, post_internal_api, resolve_session_id


def _build_base_args(parser: argparse.ArgumentParser) -> None:
  parser.add_argument("--base-repo", default=None, help="Code repo the plan targets")
  parser.add_argument("--base-branch", default=None, help="Code branch the plan targets")
  parser.add_argument("--base-sha", default=None, help="Code sha the plan targets")


def _add_present(parser: argparse.ArgumentParser) -> None:
  parser.add_argument("--file", required=True, help="Artifact path relative to the session dir")
  parser.add_argument("--title", required=True, help="Plan title")
  _build_base_args(parser)


def _add_amend(parser: argparse.ArgumentParser) -> None:
  parser.add_argument("--file", required=True, help="Artifact path relative to the session dir")
  parser.add_argument("--plan", type=int, default=None, help="Target plan id (required when ambiguous)")
  parser.add_argument(
      "--trigger", choices=["auto_amend", "feedback"], default="feedback", help="Revision trigger (default feedback)")
  _build_base_args(parser)


def _add_approve(parser: argparse.ArgumentParser) -> None:
  parser.add_argument("--plan", type=int, default=None, help="Target plan id (required when ambiguous)")


def _add_close(parser: argparse.ArgumentParser) -> None:
  parser.add_argument("--plan", type=int, required=True, help="Target plan id")
  parser.add_argument("--as", dest="close_as", required=True, choices=["superseded", "abandoned"])


def _build_parser() -> argparse.ArgumentParser:
  parent = argparse.ArgumentParser(add_help=False)
  parent.add_argument("--session", default=None, help="Session ID (optional; auto-derived from cwd)")
  parser = argparse.ArgumentParser(prog="charliebot plan", description="Plan registry verbs")
  sub = parser.add_subparsers(dest="verb", required=True)
  _add_present(sub.add_parser("present", parents=[parent], help="Register a new plan lineage"))
  _add_amend(sub.add_parser("amend", parents=[parent], help="Append the next version to a plan lineage"))
  _add_approve(sub.add_parser("approve", parents=[parent], help="Record a takeoff"))
  _add_close(sub.add_parser("close", parents=[parent], help="Terminate a plan lineage"))
  sub.add_parser("list", parents=[parent], help="Print the session's plan registry")
  return parser


def _build_payload(verb: str, session_id: str, args: argparse.Namespace) -> dict:
  if verb == "present":
    return {
        "session_id": session_id,
        "file": args.file,
        "title": args.title,
        "base_repo": args.base_repo,
        "base_branch": args.base_branch,
        "base_sha": args.base_sha,
    }
  if verb == "amend":
    return {
        "session_id": session_id,
        "file": args.file,
        "plan_id": args.plan,
        "trigger": args.trigger,
        "base_repo": args.base_repo,
        "base_branch": args.base_branch,
        "base_sha": args.base_sha,
    }
  if verb == "approve":
    return {"session_id": session_id, "plan_id": args.plan}
  if verb == "close":
    return {"session_id": session_id, "plan_id": args.plan, "close_as": args.close_as}
  raise ValueError(f"unsupported verb: {verb!r}")


def main(argv: Sequence[str] | None = None) -> None:
  parser = _build_parser()
  args = parser.parse_args(argv if argv is not None else None)
  session_id = resolve_session_id(args.session)

  if args.verb == "list":
    result = get_api(f"/api/sessions/{session_id}/plans")
  else:
    payload = _build_payload(args.verb, session_id, args)
    result = post_internal_api(f"/api/internal/plan/{args.verb}", payload)
  print(json.dumps(result, indent=2))


if __name__ == "__main__":
  main()
