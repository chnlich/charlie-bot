"""CLI: labeled-entry memory store (query / add / lint).

Pure-local; no server dependency. The store lives at
``charliebot_home_dir() / "memory"`` (``~/.charliebot/memory/``); the home is
env-resolved (src.core.home) and no config key can move it, so the verbs read
no config file — a broken config.yaml must not block the store's own verbs.
The run-token path is config-free too: the audience resolves from the home and
the credentials file (src.core.home, src.core.credentials), never the config
model stack (the M98 invocation wall). See ``src/core/memory.py`` for the
store contract.

  charliebot memory query --topic <t> [--audience A] [--index] [--resident] [--dir D]
  charliebot memory add [--file F]
  charliebot memory lint [--dir D]

  charliebot memory proposal open | status | commit <path> --message-file F | land <sha>

The ``proposal`` verbs drive the store's PR flow (``src/core/memory_proposal.py``):
the ``proposal`` branch, worked in the sibling ``memory-proposal`` worktree,
holds the drafted curation as commits, and only ``land`` — one approved
version — fast-forwards the live checkout the sessions read.

A present CHARLIEBOT_RUN_TOKEN fixes the query's audience from the verified,
active owning Run's role: omitted or contradictory --audience cannot broaden
it, and an invalid, unknown, inactive, not-launched or wrong-instance token
fails visibly instead of falling back to operator behavior.
"""

import argparse
import re
import sys
from datetime import UTC, datetime
from pathlib import Path

from src.cli.help_formatter import CliHelpFormatter
from src.core import memory
from src.core.home import charliebot_home_dir
from src.core.run_token import load_run_token


def _memory_dir() -> Path:
  """The store root: ``<home>/memory`` (the CharlieBotConfig.memory_dir derivation)."""
  return charliebot_home_dir() / "memory"


def _sessions_root() -> Path:
  """The sessions root: ``<home>/sessions`` (the CharlieBotConfig.sessions_dir derivation)."""
  return charliebot_home_dir() / "sessions"


def main() -> None:
  parser = argparse.ArgumentParser(
      description="Labeled-entry memory store: query, add captures, and lint", formatter_class=CliHelpFormatter)
  sub = parser.add_subparsers(dest="command", required=True)

  p_query = sub.add_parser(
      "query", help="Print matched entries' full text (or index lines with --index)", formatter_class=CliHelpFormatter)
  p_query.add_argument(
      "--topic", action="append", required=True, help="Topic to match (repeatable); must exist in the vocabulary")
  p_query.add_argument(
      "--audience", default=None, choices=["master", "worker"], help="Only entries whose audience contains this")
  p_query.add_argument("--index", action="store_true", help="Print index lines only")
  p_query.add_argument("--resident", action="store_true", help="Only entries in resident topics")
  p_query.add_argument(
      "--dir",
      default=None,
      help=
      "Store root to read (default: the live store at ~/.charliebot/memory); pass the PR worktree's path to read its drafted entries",
  )

  p_add = sub.add_parser(
      "add", help="Stage a free-form capture (never touches entries/)", formatter_class=CliHelpFormatter)
  p_add.add_argument("--file", default=None, help="Read body from file (default: stdin)")

  p_lint = sub.add_parser(
      "lint", help="Validate the store; exit nonzero on violations", formatter_class=CliHelpFormatter)
  p_lint.add_argument("--dir", default=None, help="Store root to validate (default: the live store)")

  p_proposal = sub.add_parser(
      "proposal",
      help="Drive the store's PR flow: open, status, commit one path, land one version",
      formatter_class=CliHelpFormatter)
  proposal_sub = p_proposal.add_subparsers(dest="proposal_command", required=True)
  proposal_sub.add_parser(
      "open",
      help="Ensure the proposal branch and worktree exist, aligned with the base branch",
      formatter_class=CliHelpFormatter)
  proposal_sub.add_parser("status", help="Print the PR state (read-only)", formatter_class=CliHelpFormatter)
  p_pcommit = proposal_sub.add_parser(
      "commit", help="Commit exactly one store-relative path on the proposal branch", formatter_class=CliHelpFormatter)
  p_pcommit.add_argument("path", help="Store-relative path: an entry file or topics")
  p_pcommit.add_argument("--message-file", required=True, help="File holding the commit message")
  p_pland = proposal_sub.add_parser(
      "land", help="Fast-forward the live checkout to one approved proposal commit", formatter_class=CliHelpFormatter)
  p_pland.add_argument("sha", help="The approved proposal commit SHA")

  args = parser.parse_args()
  if args.command == "query":
    _cmd_query(args)
  elif args.command == "add":
    _cmd_add(args)
  elif args.command == "lint":
    _cmd_lint(args)
  elif args.command == "proposal":
    _cmd_proposal(args)


def _cmd_query(args: argparse.Namespace) -> None:
  token = load_run_token()
  if token is not None:
    audience = _resolve_run_scoped_audience(token)
    if args.audience is not None and args.audience != audience:
      print(
          f"error: --audience {args.audience} contradicts the run credential's fixed "
          f"audience {audience!r}; a run token cannot broaden its own query",
          file=sys.stderr)
      sys.exit(1)
    args.audience = audience
  store = memory.load_store(_store_root(args))
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
  resident_names = memory.resident_topic_names(store)
  matched = []
  for e in store.entries:
    if e.topic not in wanted_topics:
      continue
    if args.audience and (e.audience is None or args.audience not in e.audience):
      continue
    if args.resident and e.topic not in resident_names:
      continue
    matched.append(e)
  matched.sort(key=memory.entry_order_key)
  if args.index:
    for e in matched:
      print(f"{e.topic}/{e.slug} · {e.title}")
    return
  if not matched:
    return
  # Synthesize the `# {title}` heading so query output stays navigable: v2
  # bodies carry no heading; legacy bodies keep their own.
  for e in matched:
    print(memory.full_text(e))


def _resolve_run_scoped_audience(token: str) -> str:
  """The audience the verified, active owning Run of *token* fixes — or a visible exit.

  Reuses the central run-identity pieces (the signature verifier and the one
  shared active-Run predicate in src.core.runs); no local re-implementation.
  A wrong-instance token names a session this home's sessions directory has
  never heard of, which is the same visible unknown-run refusal.
  """
  # Deferred off the module wall (M98): this resolution is the only asyncio consumer.
  import asyncio

  from src.core.credentials import configured_access_key
  from src.core.json_utils import load_model_meta
  from src.core.models import SessionMetadata
  from src.core.run_token import RunTokenError, verify_run_token
  from src.core.runs import METADATA_NAME, RunStore, run_identity_refusal
  from src.core.session_aliases import SessionAliasStore
  root = _sessions_root()
  key = configured_access_key()
  if not key:
    print("error: run token presented but no signing key is configured", file=sys.stderr)
    sys.exit(1)
  try:
    claims = verify_run_token(token, key)
  except RunTokenError as e:
    print(f"error: invalid run token: {e}", file=sys.stderr)
    sys.exit(1)
  # A read-only local resolution through the same owners the server uses (the
  # one shared active-Run predicate), no server process needed. events=None:
  # the store reads the live chat log directly (the sink's SessionManager read
  # serves the same file); nothing here writes.
  store = RunStore(root, asyncio.Lock(), None, SessionAliasStore(root))
  run = store.read_run_sync(claims.session_id, claims.run_id)
  refusal = run_identity_refusal(run, store.load_events_sync(claims.session_id))
  if refusal is not None:
    print(f"error: {refusal}", file=sys.stderr)
    sys.exit(1)
  meta = load_model_meta(root / claims.session_id / METADATA_NAME, SessionMetadata)
  if meta is None or meta.profile is None:
    print(
        f"error: run token references session {claims.session_id}, which is not a "
        "task-tree node in this instance",
        file=sys.stderr)
    sys.exit(1)
  return "master" if meta.profile == "manager" else "worker"


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
  home = charliebot_home_dir()
  sess8 = _session_slug8(home / "sessions")
  ts = datetime.now(UTC).strftime("%Y%m%dT%H%M%SZ")
  filename = f"{ts}-{sess8}-{slug}.md"
  staging_dir = home / "memory" / "staging"
  staging_dir.mkdir(parents=True, exist_ok=True)
  target = staging_dir / filename
  target.write_text(body, encoding="utf-8")
  print(str(target))


def _store_root(args: argparse.Namespace) -> Path:
  """The store root this invocation reads: --dir when given, else the live store."""
  if getattr(args, "dir", None):
    return Path(args.dir).expanduser()
  return _memory_dir()


def _cmd_lint(args: argparse.Namespace) -> None:
  violations = memory.lint(_store_root(args))
  if violations:
    for v in violations:
      print(v)
    sys.exit(1)
  print("clean")


def _cmd_proposal(args: argparse.Namespace) -> None:
  # Deferred off the module wall: only the proposal verbs import the store's PR
  # machinery (tarfile + subprocess ride its import chain); query/add/lint never
  # touch it.
  from src.core import memory_proposal

  live = _memory_dir()
  try:
    if args.proposal_command == "open":
      fields = memory_proposal.open_proposal(live)
    elif args.proposal_command == "status":
      fields = memory_proposal.status(live)
    elif args.proposal_command == "commit":
      sha = memory_proposal.commit(live, args.path, Path(args.message_file).expanduser())
      fields = {"committed": sha}
    else:
      fields = memory_proposal.land(live, args.sha)
  except memory_proposal.ProposalRefusalError as e:
    print(f"error: {e}", file=sys.stderr)
    sys.exit(1)
  for key, value in fields.items():
    print(f"{key}: {value}")


def _slugify(text: str) -> str:
  s = text.lower()
  s = re.sub(r"[^a-z0-9._-]+", "-", s)
  s = re.sub(r"-{2,}", "-", s)
  return s.strip("-")


def _session_slug8(sessions_dir: Path) -> str:
  """First 8 chars of the CharlieBot session id derived from cwd, else 'nosess'."""
  cwd = Path.cwd().resolve()
  if cwd.parent == sessions_dir.resolve():
    return cwd.name[:8]
  return "nosess"


if __name__ == "__main__":
  main()
