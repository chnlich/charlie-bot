"""CLI verbs for a session's own Slack thread, callable from a Slack-summoned session.

  charliebot slack reply --file <path>            (``-`` reads the reply from stdin)
  charliebot slack ack --message-id <ts> [...]

``reply`` posts the file's text to the thread the session was summoned from,
through the internal slack/reply endpoint, and prints the server's readback as
one JSON line: ``posted``, ``chars``, ``chunks``, ``over_budget`` (past the
500-character reply budget) and ``answers`` (the summon event id the reply
answers, or null for a round no summon started). A refusal (unread eligible
thread messages → the 412 ``stale_thread`` payload, no Slack thread, blank
text, Slack rejected the post) exits non-zero with a JSON error on stderr and
persists nothing. ``ack`` marks the given thread messages (Slack ts) as read,
advancing the session's read watermark, and prints the readback JSON
(``acked``, ``watermark_ts``); every read message's id must be passed — none
may be skipped. The session is derived from cwd per the usual CLI convention;
the reply-format contract is prompts/slack_reply_format.md.
"""

import argparse
import json
import sys

from src.cli.common import (
  add_session_arg,
  exit_usage_error,
  post_internal_api,
  read_required_text_file,
  resolve_session_id,
)


def _build_parser() -> argparse.ArgumentParser:
  parser = argparse.ArgumentParser(description="CharlieBot Slack thread verbs")
  sub = parser.add_subparsers(dest="slack_command", required=True)

  reply = sub.add_parser("reply", help="Post a reply to this session's Slack thread")
  reply.add_argument("--file", required=True, help="File holding the reply text; - reads stdin")
  add_session_arg(reply)

  ack = sub.add_parser("ack", help="Mark read thread messages, advancing the read watermark")
  ack.add_argument(
      "--message-id",
      nargs="+",
      required=True,
      metavar="TS",
      help="Slack ts of each read message; every read id at or below the newest must be included")
  add_session_arg(ack)
  return parser


def _read_reply_text(file_arg: str) -> str:
  if file_arg == "-":
    text = sys.stdin.read()
    if not text.strip():
      exit_usage_error("--file - read no text from stdin")
    return text
  return read_required_text_file("--file", file_arg)


def _cmd_reply(args: argparse.Namespace) -> None:
  session_id = resolve_session_id(args.session)
  text = _read_reply_text(args.file)
  result = post_internal_api("/api/internal/slack/reply", {"session_id": session_id, "text": text})
  print(json.dumps(result))


def _cmd_ack(args: argparse.Namespace) -> None:
  session_id = resolve_session_id(args.session)
  result = post_internal_api("/api/internal/slack/ack",
                             {"session_id": session_id, "message_ids": args.message_id})
  print(json.dumps(result))


def main() -> None:
  parser = _build_parser()
  args = parser.parse_args()
  if args.slack_command == "reply":
    _cmd_reply(args)
  elif args.slack_command == "ack":
    _cmd_ack(args)
  else:
    parser.error(f"unknown slack command: {args.slack_command}")


if __name__ == "__main__":
  main()
