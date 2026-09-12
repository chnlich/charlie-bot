"""CLI: labeled-entry memory store (query / add / lint / replay / compare).

Pure-local; no server dependency. The store lives at ``cfg.memory_dir``
(``~/.charliebot/memory/``). See ``src/core/memory.py`` for the store contract.

  charliebot memory query --topic <t> [--audience A] [--index] [--resident]
  charliebot memory add [--file F]
  charliebot memory lint
  charliebot memory replay --input <manifest> --output-dir <dir> --backend <id> \
      --mode editor-only|editor-review
  charliebot memory compare --run-dir <editor-review-run> --output-dir <dir>

``replay`` runs the isolated offline curation pipeline over a frozen manifest
(docs/memory-replay.md); it never reads or writes the live store. ``compare``
derives both comparison arms of one recorded editor-review run from the same
recorded editor response — offline, with no model calls.
"""

import argparse
import re
import sys
from datetime import UTC, datetime
from pathlib import Path

from src.core import memory
from src.core.config import CharlieBotConfig, get_config
from src.core.memory_replay import (
    MODES,
    CompareOptions,
    ReplayError,
    ReplayOptions,
    run_comparison,
    run_replay,
)


def main() -> None:
  parser = argparse.ArgumentParser(description="Labeled-entry memory store: query, add captures, and lint")
  sub = parser.add_subparsers(dest="command", required=True)

  p_query = sub.add_parser("query", help="Print matched entries' full text (or index lines with --index)")
  p_query.add_argument(
      "--topic", action="append", required=True, help="Topic to match (repeatable); must exist in the vocabulary")
  p_query.add_argument(
      "--audience", default=None, choices=["master", "worker"], help="Only entries whose audience contains this")
  p_query.add_argument("--index", action="store_true", help="Print index lines only")
  p_query.add_argument("--resident", action="store_true", help="Only entries in resident topics")

  p_add = sub.add_parser("add", help="Stage a free-form capture (never touches entries/)")
  p_add.add_argument("--file", default=None, help="Read body from file (default: stdin)")

  sub.add_parser("lint", help="Validate the store; exit nonzero on violations")

  p_replay = sub.add_parser(
      "replay", help="Run the isolated offline curation replay over a frozen manifest (never touches the store)")
  p_replay.add_argument(
      "--input", required=True, metavar="MANIFEST", help="Replay manifest (YAML); format in docs/memory-replay.md")
  p_replay.add_argument(
      "--output-dir",
      required=True,
      metavar="DIR",
      help="Output root for the proposal bundle and run records; must not overlap the store or the manifest inputs")
  p_replay.add_argument("--backend", required=True, metavar="ID", help="Configured backend id from backends.options")
  p_replay.add_argument("--mode", required=True, choices=list(MODES), help="Model stages to run")

  p_compare = sub.add_parser(
      "compare", help="Compare an editor-review run against its own editor-only alternative (offline; no model calls)")
  p_compare.add_argument(
      "--run-dir",
      required=True,
      metavar="RUN_DIR",
      help="Recorded editor-review run directory (the one containing run.json); see docs/memory-replay.md")
  p_compare.add_argument(
      "--output-dir",
      required=True,
      metavar="DIR",
      help="Output root for comparison.json and report.html; must not overlap the run dir or the store")

  args = parser.parse_args()
  if args.command == "query":
    _cmd_query(args)
  elif args.command == "add":
    _cmd_add(args)
  elif args.command == "lint":
    _cmd_lint()
  elif args.command == "replay":
    _cmd_replay(args)
  elif args.command == "compare":
    _cmd_compare(args)


def _cmd_query(args: argparse.Namespace) -> None:
  cfg = get_config()
  memory_dir = cfg.memory_dir
  store = memory.load_store(memory_dir)
  unknown = [t for t in args.topic if t not in store.topics]
  if unknown:
    for value in unknown:
      pre_slash = value.split("/", 1)[0] if "/" in value else None
      if pre_slash is not None and pre_slash in store.topics:
        print(f"error: unknown topic: {value} (index lines are topic/slug; try --topic {pre_slash})", file=sys.stderr)
      else:
        print(f"error: unknown topic: {value}", file=sys.stderr)
    sys.exit(1)
  wanted_topics = set(args.topic)
  resident_names = {t.name for t in store.topics.values() if t.resident}
  matched = []
  for e in store.entries:
    if e.topic not in wanted_topics:
      continue
    if args.audience and (e.audience is None or args.audience not in e.audience):
      continue
    if args.resident and e.topic not in resident_names:
      continue
    matched.append(e)
  matched.sort(key=lambda e: (e.topic, e.slug))
  if args.index:
    for e in matched:
      print(f"{e.topic}/{e.slug} · {e.title}")
    return
  if not matched:
    return
  # Synthesize the `# {title}` heading so query output stays navigable now
  # that v2 bodies no longer carry it; legacy bodies keep their own heading.
  for e in matched:
    print(memory.full_text(e))


def _cmd_add(args: argparse.Namespace) -> None:
  body = Path(args.file).read_text(encoding="utf-8") if args.file else sys.stdin.read()
  lines = body.split("\n")
  if not lines or not lines[0].startswith("# "):
    print("error: body must start with '# <title>'", file=sys.stderr)
    sys.exit(1)
  title = lines[0][2:].strip()
  if not title:
    print("error: empty title after '# '", file=sys.stderr)
    sys.exit(1)
  # A title with no slug-charset character (pure CJK, for example) falls back
  # to the fixed ``capture`` segment; the write still proceeds.
  slug = _slugify(title) or "capture"
  cfg = get_config()
  sess8 = _session_slug8(cfg)
  ts = datetime.now(UTC).strftime("%Y%m%dT%H%M%SZ")
  filename = f"{ts}-{sess8}-{slug}.md"
  staging_dir = cfg.memory_dir / "staging"
  staging_dir.mkdir(parents=True, exist_ok=True)
  target = staging_dir / filename
  target.write_text(body, encoding="utf-8")
  print(str(target))


def _cmd_lint() -> None:
  cfg = get_config()
  violations = memory.lint(cfg.memory_dir)
  if violations:
    for v in violations:
      print(v)
    sys.exit(1)
  print("clean")


def _cmd_replay(args: argparse.Namespace) -> None:
  options = ReplayOptions(
      manifest=Path(args.input), output_dir=Path(args.output_dir), backend=args.backend, mode=args.mode)
  try:
    outcome = run_replay(options)
  except ReplayError as e:
    print(f"error: {e}", file=sys.stderr)
    sys.exit(1)
  prefix = "reused completed run" if outcome.reused else "replay complete"
  print(f"{prefix}: {args.mode}")
  print(f"run: {outcome.run_dir}")
  print(f"proposal: {outcome.proposal_path}")
  print(f"report: {outcome.report_path}")
  print(
      f"dispositions: propose {outcome.propose}, no_change {outcome.no_change}, "
      f"needs_decision {outcome.needs_decision}")
  print(f"changed paths: {len(outcome.changed_paths)}")


def _cmd_compare(args: argparse.Namespace) -> None:
  options = CompareOptions(run_dir=Path(args.run_dir), output_dir=Path(args.output_dir))
  try:
    outcome = run_comparison(options)
  except ReplayError as e:
    print(f"error: {e}", file=sys.stderr)
    sys.exit(1)
  print("comparison complete: paired editor/reviewer (no model calls)")
  print(f"source run: {args.run_dir}")
  print(f"comparison: {outcome.comparison_path}")
  print(f"report: {outcome.report_path}")
  print(f"arms: editor-only {outcome.editor_status}, post-review {outcome.reviewer_status}")


def _slugify(text: str) -> str:
  s = text.lower()
  s = re.sub(r"[^a-z0-9._-]+", "-", s)
  s = re.sub(r"-{2,}", "-", s)
  return s.strip("-")


def _session_slug8(cfg: CharlieBotConfig) -> str:
  """First 8 chars of the CharlieBot session id derived from cwd, else 'nosess'."""
  cwd = Path.cwd().resolve()
  sessions_dir = cfg.sessions_dir.resolve()
  if cwd.parent == sessions_dir:
    return cwd.name[:8]
  return "nosess"


if __name__ == "__main__":
  main()
