"""CLI verbs for a web session's For-you layer and status, callable from the session's own shell.

  charliebot web reply --file <path>            (``-`` reads the reply from stdin)
  charliebot web dialog [--session ID]
  charliebot web status set --file <path>       (``-`` reads the JSON from stdin)
  charliebot web status show [--session ID]

``reply`` records the file's text as the session's web reply through the internal web/reply endpoint and prints the
readback as one JSON line: ``posted``, ``chars``, ``over_budget`` (past the 500-character reply budget) and
``answers`` (the user message id the reply answers, or null). A refusal (unknown session, a thread session or a
worker, blank text) exits non-zero with a JSON error on stderr and records nothing.

``dialog`` prints the session's For-you layer as plain text in log order: each user message as ``You:``, each reply
as ``Reply:`` and each notice as ``Notice:``. The session resolves per ``resolve_session_id``; the reply rules are
prompts/web_reply.md.

``status set`` stores the items that the file's JSON object names (any of ``goal``, ``now``, ``waiting``) through the
internal web/status endpoint and prints the stored status as one JSON line. A refusal exits non-zero with a JSON error
on stderr and stores nothing. ``status show`` prints the session's status as plain text, then the waiting items of
its open child sessions, one block per child.
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

  status = sub.add_parser(
      "status", help="Set or show this session's status", formatter_class=help_formatter.CliHelpFormatter)
  status_sub = status.add_subparsers(dest="status_command", required=True)
  status_set = status_sub.add_parser(
      "set", help="Store the status items that a JSON file names", formatter_class=help_formatter.CliHelpFormatter)
  status_set.add_argument("--file", required=True, help="JSON object with any of goal, now, waiting; - reads stdin")
  common.add_session_arg(status_set)
  status_show = status_sub.add_parser(
      "show",
      help="Print this session's status and the waiting items of its child sessions",
      formatter_class=help_formatter.CliHelpFormatter)
  common.add_session_arg(status_show)
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


def _cmd_status_set(args: argparse.Namespace) -> None:
  session_id = common.resolve_session_id(args.session)
  text = common.read_reply_text(args.file)
  result = common.post_internal_api("/api/internal/web/status", {"session_id": session_id, "text": text})
  print(json.dumps(result))


def _item_line(item: dict) -> str:
  return f"- {item['text']}" + (f" ({item['link']})" if item["link"] else "")


def _render_status(result: dict) -> str:
  """The GET result as plain text: the session's Goal, Now and Waiting on you, then one block per child session."""
  lines: list[str] = []
  status = result["web_status"]
  if status is None:
    lines.append("No status yet")
  else:
    lines.append(f"Goal: {status['goal'] or '(not set)'}")
    lines.append(f"Now: {status['now'] or '(not set)'} (updated {status['updated_at']})")
    if status["waiting"]:
      lines.append("Waiting on you:")
      lines.extend(_item_line(item) for item in status["waiting"])
    else:
      lines.append("Waiting on you: Nothing")
  for row in result["descendants"]:
    lines.append("")
    lines.append(f"{row['name']} ({row['session_id'][:8]})")
    lines.extend(_item_line(item) for item in row["waiting"])
  return "\n".join(lines)


def _cmd_status_show(args: argparse.Namespace) -> None:
  session_id = common.resolve_session_id(args.session)
  result = common.get_api("/api/internal/web/status", {"session_id": session_id})
  print(_render_status(result))


def main() -> None:
  parser = _build_parser()
  args = parser.parse_args()
  if args.web_command == "reply":
    _cmd_reply(args)
  elif args.web_command == "dialog":
    _cmd_dialog(args)
  elif args.web_command == "status" and args.status_command == "set":
    _cmd_status_set(args)
  elif args.web_command == "status" and args.status_command == "show":
    _cmd_status_show(args)
  else:
    parser.error(f"unknown web command: {args.web_command}")


if __name__ == "__main__":
  main()
