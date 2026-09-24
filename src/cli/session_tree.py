"""Session-tree CLI: the migration and preview verbs of the task-tree line.

  charliebot session-tree migrate --dry-run --output FILE
  charliebot session-tree migrate --apply --manifest FILE
  charliebot session-tree migrate --rollback --manifest FILE
  charliebot session-tree preview --home DIR --port PORT [--backend ID] \
      [--add-backend ID ...]

The migrate commands target the current explicitly selected CHARLIEBOT_HOME and
touch nothing else: no HTTP delegation, no server startup, no external messages.
``--dry-run`` reads the source read-only and writes the reviewable manifest to
FILE (FILE must live outside the home: a manifest inside the inventoried home
would overwrite a source or hide a new file in its own input set);
``--apply`` refuses unresolved conversions, source/converter hash drift,
unaccounted files, target collisions and unproven quiescence before its first
replacement, keeps a verified backup before mutating, and re-verifies through
the ordinary readers after the write; ``--rollback`` re-validates the whole
protected home under the writer fence before its first restore, restores
originals and removes only migration-owned unchanged products, and refuses
once anything in the home is no longer accounted for by the manifest's
binding and receipts.

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
real-data offline rehearsal, real session import into a preview home, runtime
cutover integration, and production apply authorization.
"""

import argparse
import json
import sys
from collections import Counter
from pathlib import Path

from src.core.config import get_config
from src.core.session_tree_migration import (
  MigrationError,
  MigrationRefusedError,
  apply_manifest,
  build_manifest,
  rollback_manifest,
  scan_source,
)


def _build_parser() -> argparse.ArgumentParser:
  parser = argparse.ArgumentParser(
      prog="charliebot session-tree",
      description="Session task-tree maintenance commands")
  sub = parser.add_subparsers(dest="session_tree_command", required=True)

  migrate = sub.add_parser(
      "migrate",
      help="Convert the selected home's legacy sessions to the task tree",
      description=(
          "Inventory the selected CHARLIEBOT_HOME and convert it to the "
          "session task tree. --dry-run writes a reviewable manifest; --apply "
          "executes one reviewed manifest under the stopped-writer boundary; "
          "--rollback restores a manifest's originals."))
  mode = migrate.add_mutually_exclusive_group(required=True)
  mode.add_argument("--dry-run", action="store_true",
                    help="Read-only: build the manifest and write it to --output")
  mode.add_argument("--apply", action="store_true",
                    help="Apply the reviewed manifest at --manifest")
  mode.add_argument("--rollback", action="store_true",
                    help="Roll back the applied manifest at --manifest")
  migrate.add_argument("--output", default=None,
                       help="Manifest output path (--dry-run)")
  migrate.add_argument("--manifest", default=None,
                       help="Manifest path (--apply / --rollback)")

  preview = sub.add_parser(
      "preview",
      help="Start an isolated session-tree trial instance in the foreground",
      description=(
          "Prepare, validate and run one isolated trial instance of the real "
          "application on its own home and loopback port. A fresh home is "
          "seeded with minimal private config and its own random access key; "
          "an existing validated preview home is reused with its tasks and "
          "config. Existing legacy/migrated state, unrelated configurations, "
          "overlapping paths, occupied ports and unprovable instance ownership "
          "refuse before any write."))
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


def _cmd_migrate(args: argparse.Namespace) -> None:
  cfg = get_config()
  if args.dry_run:
    if not args.output:
      _fail("--dry-run requires --output FILE")
    # The "read-only inventory" must not write inside its own input: an output
    # inside the home would overwrite an existing source or plant a new file
    # the manifest's own binding would then have to account for.
    output = Path(args.output)
    try:
      if output.resolve().is_relative_to(cfg.charliebot_home.resolve()):
        _fail(
            f"--output {args.output!r} is inside the selected home "
            f"({cfg.charliebot_home}); the manifest must live outside the home it inventories")
    except OSError as e:
      _fail(f"--output path is unusable: {e}")
    try:
      snap = scan_source(cfg)
      manifest, plan = build_manifest(cfg, snap)
      if output.parent and not output.parent.exists():
        output.parent.mkdir(parents=True, exist_ok=True)
      from src.core.json_utils import atomic_write_text
      atomic_write_text(output, manifest.model_dump_json(indent=2))
    except MigrationRefusedError as e:
      _fail(str(e), e.details)
    except OSError as e:
      _fail(f"dry-run output could not be written to {output}: {e}")
    _emit({
        "status": "dry_run",
        "manifest": str(output),
        "source_sha": manifest.source_sha,
        "mappings": len(manifest.mappings),
        "unresolved": [u.model_dump() for u in manifest.unresolved],
        "unresolved_count": len(manifest.unresolved),
        "import_report_count": len(manifest.import_report),
        "import_report_by_kind": dict(sorted(
            Counter(entry.source_kind for entry in manifest.import_report).items())),
        "pending_inputs": sum(len(m.pending_inputs) for m in plan.managers),
        "input_summary": plan.input_summary,
        "organization_pending": plan.organization_pending,
    })
    if manifest.unresolved:
      sys.exit(1)
    return

  if args.apply:
    if not args.manifest:
      _fail("--apply requires --manifest FILE")
    try:
      result = apply_manifest(cfg, Path(args.manifest))
    except MigrationRefusedError as e:
      _fail(str(e), e.details)
    except MigrationError as e:
      _fail(str(e))
    _emit(result)
    return

  if not args.manifest:
    _fail("--rollback requires --manifest FILE")
  try:
    result = rollback_manifest(cfg, Path(args.manifest))
  except MigrationRefusedError as e:
    _fail(str(e), e.details)
  except MigrationError as e:
    _fail(str(e))
  _emit(result)


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
  if args.session_tree_command == "migrate":
    _cmd_migrate(args)
  elif args.session_tree_command == "preview":
    _cmd_preview(args)


if __name__ == "__main__":
  main()
