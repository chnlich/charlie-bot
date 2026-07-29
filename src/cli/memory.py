"""CLI: labeled-entry memory store (query / add / lint / usage).

Pure-local; no server dependency. The store lives at ``cfg.memory_dir``
(``~/.charliebot/memory/``). See ``src/core/memory.py`` for the store contract.

  charliebot memory query --topic <t> [--audience A] [--index] [--resident]
  charliebot memory add --topic <t> --scope <s> --audience <a> [--revises SLUG] [--file F | stdin]
  charliebot memory lint
  charliebot memory usage [--idle-days N]
"""

import argparse
import re
import sys
from datetime import datetime, timezone
from pathlib import Path

from src.core import memory
from src.core.config import get_config

_TOPIC_NAME_RE = re.compile(r"^[a-z0-9][a-z0-9-]*$")


def main() -> None:
  parser = argparse.ArgumentParser(
      description="Labeled-entry memory store: query, add candidates, lint, and usage stats")
  sub = parser.add_subparsers(dest="command", required=True)

  p_query = sub.add_parser("query", help="Print matched entries' full text (or index lines with --index)")
  p_query.add_argument(
      "--topic", action="append", required=True, help="Topic to match (repeatable); must exist in the vocabulary")
  p_query.add_argument("--audience", default=None, choices=["master", "worker", "both"])
  p_query.add_argument("--index", action="store_true", help="Print index lines only; no usage recorded")
  p_query.add_argument("--resident", action="store_true", help="Only entries in resident topics")

  p_add = sub.add_parser("add", help="Stage a candidate entry (never touches entries/)")
  p_add.add_argument("--topic", required=True, help="Topic slug; need not exist in the vocabulary yet")
  p_add.add_argument("--scope", required=True, choices=["user", "host"])
  p_add.add_argument("--audience", required=True, choices=["master", "worker", "both"])
  p_add.add_argument("--revises", default=None, help="Slug of an existing entry in this topic this candidate revises")
  p_add.add_argument("--file", default=None, help="Read body from file (default: stdin)")

  sub.add_parser("lint", help="Validate the store; exit nonzero on violations")

  p_usage = sub.add_parser("usage", help="Print per-entry usage stats")
  p_usage.add_argument(
      "--idle-days", type=int, default=60, dest="idle_days", help="Idle threshold in days (default 60)")

  args = parser.parse_args()
  if args.command == "query":
    _cmd_query(args)
  elif args.command == "add":
    _cmd_add(args)
  elif args.command == "lint":
    _cmd_lint()
  elif args.command == "usage":
    _cmd_usage(args)


def _cmd_query(args: argparse.Namespace) -> None:
  cfg = get_config()
  memory_dir = cfg.memory_dir
  store = memory.load_store(memory_dir)
  unknown = [t for t in args.topic if t not in store.topics]
  if unknown:
    print(f"error: unknown topic: {', '.join(unknown)}", file=sys.stderr)
    sys.exit(1)
  wanted_topics = set(args.topic)
  resident_names = {t.name for t in store.topics.values() if t.resident}
  matched = []
  for e in store.entries:
    if e.topic not in wanted_topics:
      continue
    if args.audience and e.audience != args.audience:
      continue
    if args.resident and e.topic not in resident_names:
      continue
    matched.append(e)
  matched.sort(key=lambda e: (e.topic, e.slug))
  if args.index:
    for e in matched:
      print(f"{e.topic}/{e.slug} \u00b7 {e.title}")
    return
  if not matched:
    return
  for e in matched:
    print(e.body.rstrip("\n"))
  memory.record_usage(memory_dir, memory.CALLER_QUERY, [e.id for e in matched])


def _cmd_add(args: argparse.Namespace) -> None:
  if not _TOPIC_NAME_RE.match(args.topic):
    print(f"error: --topic {args.topic!r} is not a valid topic name (lowercase, digits, hyphens)", file=sys.stderr)
    sys.exit(1)
  if args.revises is not None and not memory._SLUG_RE.match(args.revises):
    print(f"error: --revises {args.revises!r} does not match slug charset [A-Za-z0-9._-]", file=sys.stderr)
    sys.exit(1)
  if args.file:
    body = Path(args.file).read_text(encoding="utf-8")
  else:
    body = sys.stdin.read()
  lines = body.split("\n")
  if not lines or not lines[0].startswith("# "):
    print("error: body must start with '# <title>'", file=sys.stderr)
    sys.exit(1)
  title = lines[0][2:].strip()
  if not title:
    print("error: empty title after '# '", file=sys.stderr)
    sys.exit(1)
  slug = _slugify(title)
  if not slug:
    print(f"error: could not derive a slug from title {title!r}", file=sys.stderr)
    sys.exit(1)
  cfg = get_config()
  sess8 = _session_slug8(cfg)
  ts = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
  today = datetime.now(timezone.utc).strftime("%Y-%m-%d")
  filename = f"{ts}-{sess8}-{slug}.md"
  header = [
      "---",
      f"topic: {args.topic}",
      f"scope: {args.scope}",
      f"audience: {args.audience}",
      f"created: {today}",
      f"source: cli-{sess8}",
  ]
  if args.revises:
    header.append(f"revises: {args.revises}")
  header.append("---")
  content = "\n".join(header) + "\n" + body
  if not content.endswith("\n"):
    content += "\n"
  staging_dir = cfg.memory_dir / "staging"
  staging_dir.mkdir(parents=True, exist_ok=True)
  target = staging_dir / filename
  target.write_text(content, encoding="utf-8")
  print(str(target))


def _cmd_lint() -> None:
  cfg = get_config()
  violations = memory.lint(cfg.memory_dir)
  if violations:
    for v in violations:
      print(v)
    sys.exit(1)
  print("clean")


def _cmd_usage(args: argparse.Namespace) -> None:
  cfg = get_config()
  stats = memory.usage_stats(cfg.memory_dir, idle_days=args.idle_days)
  rows = [["entry", "hits", "last_seen", "idle_days", "over_threshold"]]
  for s in stats:
    rows.append(
        [
            s.entry_id,
            str(s.hits),
            s.last_seen or "(never)",
            "-" if s.idle_days is None else str(s.idle_days),
            str(s.over_threshold),
        ])
  _print_table(rows)


def _print_table(rows: list[list[str]]) -> None:
  widths = [max(len(r[i]) for r in rows) for i in range(len(rows[0]))]
  for r in rows:
    print("  ".join(c.ljust(widths[i]) for i, c in enumerate(r)))


def _slugify(text: str) -> str:
  s = text.lower()
  s = re.sub(r"[^a-z0-9._-]+", "-", s)
  s = re.sub(r"-{2,}", "-", s)
  return s.strip("-")


def _session_slug8(cfg) -> str:
  """First 8 chars of the CharlieBot session id derived from cwd, else 'nosess'."""
  cwd = Path.cwd().resolve()
  sessions_dir = cfg.sessions_dir.resolve()
  if cwd.parent == sessions_dir:
    return cwd.name[:8]
  return "nosess"


if __name__ == "__main__":
  main()
