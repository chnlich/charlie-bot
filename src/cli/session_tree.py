"""Session-tree CLI: the preview verb of the task-tree line.

  charliebot session-tree preview --home DIR --port PORT [--backend ID] \
      [--add-backend ID ...]

``preview`` prepares, validates and runs one isolated trial instance of the
real application in the foreground (src/core/session_tree_preview.py owns the
contract); it never reads or writes the source profile's sessions, memory,
schedules, triggers, native sessions or operator key. ``--add-backend``
(repeatable) extends the trial's explicitly selected charlie-code catalog:
fresh homes seed with all requested entries, an existing validated preview
home gains the requested entries under its writer fence after every requested
entry and referenced credential validates, and a restart without the flag
keeps the stored catalog and everything else exactly as it is.

Every refusal exits 1 with a structured JSON diagnostic on stderr — never an
uncaught traceback. Exit code 0 success, 1 refusal/conflict, 2 usage.

Not yet part of this command (separate owners, do not assume them here): the
real-data offline rehearsal and the production runtime cutover.
"""

import argparse
import json
import sys


def _build_parser() -> argparse.ArgumentParser:
  parser = argparse.ArgumentParser(
      prog="charliebot session-tree",
      description="Session task-tree maintenance commands")
  sub = parser.add_subparsers(dest="session_tree_command", required=True)

  preview = sub.add_parser(
      "preview",
      help="Start an isolated session-tree trial instance in the foreground",
      description=(
          "Prepare, validate and run one isolated trial instance of the real "
          "application on its own home and loopback port. A fresh home is "
          "seeded with minimal private config and its own random access key; "
          "an existing validated preview home is reused with its tasks and "
          "config. Existing legacy state, unrelated configurations, overlapping "
          "paths, occupied ports and unprovable instance ownership refuse "
          "before any write."))
  preview.add_argument("--home", required=True, metavar="DIR",
                       help="The preview instance's own CharlieBot home (required)")
  preview.add_argument("--port", required=True, type=int, metavar="PORT",
                       help="The preview instance's loopback port (required)")
  preview.add_argument("--backend", default=None, metavar="ID",
                       help="Backend id for initial setup (required for a fresh home; "
                            "on restart it must match the home's configured backend)")
  preview.add_argument("--add-backend", dest="add_backend", action="append", default=None,
                       metavar="ID",
                       help="Additional charlie-code backend id to add to this trial's "
                            "explicitly selected catalog (repeatable). Fresh homes seed "
                            "with every requested entry; an existing preview home gains "
                            "the requested entries after they and their credentials "
                            "validate, under the home writer fence. A restart without "
                            "the flag keeps the stored catalog unchanged.")
  return parser


def _emit(payload: dict) -> None:
  print(json.dumps(payload, indent=2, default=str))


def _fail(message: str, details: list[str] | None = None) -> None:
  payload = {"error": message}
  if details:
    payload["details"] = details
  print(json.dumps(payload, indent=2), file=sys.stderr)
  sys.exit(1)


def _cmd_preview(args: argparse.Namespace) -> None:
  from src.core.home_writer_fence import HomeWriterActiveError
  from src.core.session_tree_preview import PreviewRefusedError, run_preview_command

  try:
    run_preview_command(args.home, args.port, args.backend, args.add_backend or [])
  except PreviewRefusedError as e:
    _fail(str(e), e.details)
  except HomeWriterActiveError as e:
    # A live holder (the running preview instance itself) refuses the whole
    # launch, additions included, as one structured diagnostic: never a
    # traceback, never a partial mutation.
    _fail(str(e))


def main() -> None:
  parser = _build_parser()
  args = parser.parse_args()
  if args.session_tree_command == "preview":
    _cmd_preview(args)


if __name__ == "__main__":
  main()
