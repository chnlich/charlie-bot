"""CLI verbs for a web session's For-you layer, callable from the session's own shell.

  charliebot web reply --file <path>            (``-`` reads the reply from stdin)
  charliebot web dialog [--session ID]

``reply`` records the file's text as the session's web reply through the internal web/reply endpoint and prints the
readback as one JSON line: ``posted``, ``chars``, ``over_budget`` (past the 500-character reply budget) and
``answers`` (the user message id the reply answers, or null). A refusal (unknown session, a thread session or a
worker, blank text) exits non-zero with a JSON error on stderr and records nothing.

``dialog`` prints the session's For-you layer as plain text in log order: each user message as ``You:``, each reply
as ``Reply:`` and each notice as ``Notice:``. The session resolves per ``resolve_session_id``; the reply rules are
prompts/web_reply.md.
"""

import argparse
import json

from src.infra import help_formatter
from src.runtime.cli import common

_LABELS = {"user": "You", "web_reply": "Reply", "web_reply_notice": "Notice"}


def _build_parser() -> argparse.ArgumentParser:
  parser = argparse.ArgumentParser(
      description="CharlieBot web session verbs", formatter_class=help_formatter.CliHelpFormatter)
  sub = parser.add_subparsers(dest="web_command", required=True)

  reply = sub.add_parser(
      "reply", help="Record a web reply for this session", formatter_class=help_formatter.CliHelpFormatter)
  reply.add_argument("--file", required=True, help="File holding the reply text; - reads stdin")
  common.add_session_arg(reply)

  dialog = sub.add_parser(
      "dialog",
      help="Print this session's For-you layer as plain text",
      formatter_class=help_formatter.CliHelpFormatter)
  common.add_session_arg(dialog)
  return parser


def _cmd_reply(args: argparse.Namespace) -> None:
  session_id = common.resolve_session_id(args.session)
  text = common.read_reply_text(args.file)
  result = common.post_internal_api("/api/internal/web/reply", {"session_id": session_id, "text": text})
  print(json.dumps(result))


def _cmd_dialog(args: argparse.Namespace) -> None:
  session_id = common.resolve_session_id(args.session)
  result = common.get_api("/api/internal/web/dialog", {"session_id": session_id})
  blocks = [f"{_LABELS[entry['role']]}: {entry['content']}" for entry in result["entries"]]
  if blocks:
    print("\n\n".join(blocks))


def main() -> None:
  parser = _build_parser()
  args = parser.parse_args()
  if args.web_command == "reply":
    _cmd_reply(args)
  elif args.web_command == "dialog":
    _cmd_dialog(args)
  else:
    parser.error(f"unknown web command: {args.web_command}")


if __name__ == "__main__":
  main()
