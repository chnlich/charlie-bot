"""CLI verbs for a web session's For-you layer and status, callable from the session's own shell.

  charliebot web reply --file <path> [--keep-language <reason>]   (``-`` reads the reply from stdin)
  charliebot web dialog [--session ID]
  charliebot web status set --file <path>       (``-`` reads JSON from stdin; cold reads exit 3 without storing)
  charliebot web status confirm
  charliebot web status show [--session ID]

``reply`` records the file's text as the session's web reply through the internal web/reply endpoint and prints the
readback as one JSON line: ``posted``, ``chars``, ``over_budget`` (past the 500-character reply budget) and
``answers`` (the user message id the reply answers, or null). A refusal (unknown session, a thread session or a
worker, blank text, an empty reason) exits non-zero with a JSON error on stderr and records nothing. The language
check holds a reply when the user's last message is Chinese and the reply has almost no Chinese; a held reply takes
the same refusal path, and its error says how to continue. ``--keep-language <reason>`` posts the reply as written,
skips the check, and records the reason on the reply.

``dialog`` prints the session's For-you layer as plain text in log order: each user message as ``You:``, each reply
as ``Reply:`` and each notice as ``Notice:``. The session resolves per ``resolve_session_id``; the reply rules are
prompts/web_reply.md.

``status set`` first refuses a Waiting on you item whose plan link names an older version or a closed lineage. It then
checks whether each remaining item has a cold read of its current text, context, link and host-served page. It prints
each new read and exits 3 without storing; revise the items and run the command again. When every item has a read, the
command stores the named items and prints one JSON line with the status, ``now_chars`` and ``now_over_budget``. A
refusal exits non-zero with a JSON error on stderr and stores nothing. ``status confirm`` records that the stored
status needs no change; it refuses a stale stored plan link and cold reads nothing. ``status show`` prints the
session's status as plain text, including each item's context, then the waiting items of its open child sessions,
then one line of Now per open child session that holds a Now, newest first.
"""

import argparse
import json
import sys

from src.infra import help_formatter
from src.runtime import cold_read, file_urls
from src.runtime.cli import common

_LABELS = {"user": "You", "web_reply": "Reply", "web_reply_notice": "Notice"}


def _build_parser() -> argparse.ArgumentParser:
  parser = argparse.ArgumentParser(
      description="CharlieBot web session verbs", formatter_class=help_formatter.CliHelpFormatter)
  sub = parser.add_subparsers(dest="web_command", required=True)

  reply = sub.add_parser(
      "reply", help="Record a web reply for this session", formatter_class=help_formatter.CliHelpFormatter)
  common.add_reply_file_arg(reply)
  reply.add_argument(
      "--keep-language", metavar="REASON", help="Post the reply as written and record REASON; skips the language check")
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
      "set",
      help="Cold read Waiting on you items before storing the status",
      formatter_class=help_formatter.CliHelpFormatter)
  status_set.add_argument(
      "--file",
      required=True,
      help="JSON object with any of goal, now, waiting; prints cold reads and exits 3 before storing; - reads stdin")
  common.add_session_arg(status_set)
  status_sub.add_parser(
      "confirm", help="Confirm that the stored status needs no change", formatter_class=help_formatter.CliHelpFormatter)
  status_show = status_sub.add_parser(
      "show",
      help="Print this session's status and the waiting items of its child sessions",
      formatter_class=help_formatter.CliHelpFormatter)
  common.add_session_arg(status_show)
  return parser


def _cmd_reply(args: argparse.Namespace) -> None:
  session_id = common.resolve_session_id(args.session)
  text = common.read_reply_text(args.file)
  payload = {"session_id": session_id, "text": text}
  if args.keep_language is not None:
    payload["keep_language"] = args.keep_language
  result = common.post_internal_api("/api/internal/web/reply", payload)
  common.print_json(result, indent=None, file=sys.stdout)


def _cmd_dialog(args: argparse.Namespace) -> None:
  session_id = common.resolve_session_id(args.session)
  result = common.get_api("/api/internal/web/dialog", {"session_id": session_id})
  blocks = [f"{_LABELS[entry['role']]}: {entry['content']}" for entry in result["entries"]]
  if blocks:
    print("\n\n".join(blocks))


def _cmd_status_set(args: argparse.Namespace) -> None:
  session_id = common.resolve_session_id(args.session)
  text = common.read_reply_text(args.file)
  request = {"session_id": session_id, "text": text}
  unread = common.post_internal_api("/api/internal/web/status/unread", request)["unread"]
  if not unread:
    result = common.post_internal_api("/api/internal/web/status", request)
    common.print_json(result, indent=None, file=sys.stdout)
    return

  cfg = common.get_config()
  system_prompt, questions = cold_read.load_ask_texts(cfg.charlie_bot_repo)
  for item in unread:
    path = file_urls.host_file(item["link"])
    linked_page = None if path is None else path.read_text(encoding="utf-8")
    probe_prompt = cold_read.ask_probe_input(item, linked_page, questions)
    probe = cold_read.run_light_probe(cfg, probe_prompt, system_prompt)
    if probe.backend_id is None:
      for backend, error in probe.attempts:
        print(f"attempt {backend} failed: {error}", file=sys.stderr)
      if not probe.attempts:
        print("No light backend was available for this item.", file=sys.stderr)
      sys.exit(1)
    assert probe.answer is not None, "a successful ask probe has the answering backend's response"
    common.post_internal_api(
        "/api/internal/web/ask-check", {
            "session_id": session_id,
            "item": item,
            "backend": probe.backend_id,
            "answer": probe.answer,
        })
    print(f"Waiting on you: {item['text']}\nBackend: {probe.backend_id}\n{probe.answer}\n")
  print("Nothing was stored.")
  sys.exit(3)


def _cmd_status_confirm() -> None:
  session_id = common.resolve_session_id(None)
  result = common.post_internal_api("/api/internal/web/status/confirm", {"session_id": session_id})
  common.print_json(result, indent=None, file=sys.stdout)


def _item_line(item: dict) -> str:
  return f"- {item['text']}" + (f" ({item['link']})" if item["link"] else "")


def _item_lines(item: dict) -> list[str]:
  """The item line and its context indented below it, when stored."""
  lines = [_item_line(item), f"  since: {item['since']}", f"  since_exact: {json.dumps(item['since_exact'])}"]
  context = item.get("context")
  if context is not None:
    context_lines = context.splitlines()
    lines.append(f"  context: {context_lines[0]}")
    lines.extend(f"  {line}" for line in context_lines[1:])
  return lines


def _render_status(result: dict) -> str:
  """The GET result as plain text: the session's Goal, Now and Waiting on you, one block per child with waiting items,
  then the Now of each open child."""
  lines: list[str] = []
  status = result["web_status"]
  if status is None:
    lines.append("No status yet")
  else:
    lines.append(f"Goal: {status['goal'] or '(not set)'}")
    lines.append(f"Now: {status['now'] or '(not set)'} (updated {status['updated_at']})")
    if status["waiting"]:
      lines.append("Waiting on you:")
      lines.extend(line for item in status["waiting"] for line in _item_lines(item))
    else:
      lines.append("Waiting on you: Nothing")
  for row in result["descendants"]:
    lines.append("")
    lines.append(f"{row['name']} ({row['session_id'][:8]})")
    lines.extend(line for item in row["waiting"] for line in _item_lines(item))
  if result["children"]:
    lines.append("")
    lines.append("Child sessions' Now:")
    lines.extend(
        f"{row['name']} ({row['session_id'][:8]}): {row['now']} (updated {row['updated_at']})"
        for row in result["children"])
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
  elif args.web_command == "status" and args.status_command == "confirm":
    _cmd_status_confirm()
  elif args.web_command == "status" and args.status_command == "show":
    _cmd_status_show(args)
  else:
    parser.error(f"unknown web command: {args.web_command}")


if __name__ == "__main__":
  main()
