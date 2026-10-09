"""CLI verbs for a session's own Discord thread, callable from a Discord-summoned session.

  charliebot discord reply --file <path>            (``-`` reads the reply from stdin)
  charliebot discord read [--url <discord link>] [--limit N]
  charliebot discord check

``reply`` posts the file's text to the thread the session was summoned from,
through the internal discord/reply endpoint, and prints the server's readback as
one JSON line: ``posted``, ``text`` (what actually went out — the text posts
exactly as written), ``operator_only_note``
(one line naming the application-route links that stay as written and reach the
operator alone, null when there are none), ``chars``, ``chunks``, ``over_budget``
(past the 500-character reply budget) and ``answers`` (the summon event id the
reply answers, or null for a round no summon started). A refusal (unread
eligible thread messages → the 412 ``stale_thread``
payload — run ``charliebot discord read`` first; no Discord thread → 409; blank
text or a CharlieBot file-server link → 422 naming the link and the
``charliebot publish`` command that produces the URL to write instead; Discord
rejected the post → 502) exits non-zero with a JSON error on stderr and persists
nothing — no chunk of the reply posts.

``read`` posts the session, an optional Discord link and the page size to the
internal discord/read endpoint and prints the readback JSON: ``messages`` (each
with ``id``, ``author_id``, ``author``, ``person`` (the name the
``discord.allowed_users`` map gives the author's account, null when the account
is not in the map), ``timestamp``, ``content``, ``attachments`` and ``unread``),
``watermark_id`` and ``more_unread``. Without ``--url`` it reads
the session's own thread and marks the unread messages it returned as read; with
``--url`` it reads the linked channel or thread and marks nothing. ``--limit`` is
the page size, 1..100, default 50.

``check`` posts an empty body to the internal discord/check endpoint and prints
the readback (``ok``, ``bot_user``, ``application_id``, ``message_content_intent``,
``guilds`` each with ``id``, ``name`` and ``missing_permissions``), then exits 1
when ``ok`` is false, after printing — the operator's pre-deploy setup check; it
takes no session.

``reply`` and ``read`` resolve the session per ``resolve_session_id``;
the reply-format contract is prompts/thread_reply_format.md.
"""

import argparse
import json
import sys

from src.infra import help_formatter
from src.runtime.cli import common

_READ_LIMIT_MIN = 1
_READ_LIMIT_MAX = 100
_READ_LIMIT_DEFAULT = 50


def _build_parser() -> argparse.ArgumentParser:
  parser = argparse.ArgumentParser(
      description="CharlieBot Discord thread verbs", formatter_class=help_formatter.CliHelpFormatter)
  sub = parser.add_subparsers(dest="discord_command", required=True)

  reply = sub.add_parser(
      "reply", help="Post a reply to this session's Discord thread", formatter_class=help_formatter.CliHelpFormatter)
  reply.add_argument("--file", required=True, help="File holding the reply text; - reads stdin")
  common.add_session_arg(reply)

  read = sub.add_parser(
      "read",
      help="Read thread messages, marking the returned unread ones read",
      formatter_class=help_formatter.CliHelpFormatter)
  read.add_argument(
      "--url", default=None, help="Discord link to read instead of this session's own thread; marks nothing read")
  read.add_argument(
      "--limit",
      type=int,
      default=_READ_LIMIT_DEFAULT,
      metavar="N",
      help=f"Messages per page, {_READ_LIMIT_MIN}..{_READ_LIMIT_MAX} (default {_READ_LIMIT_DEFAULT})")
  common.add_session_arg(read)

  sub.add_parser(
      "check",
      help="Verify the Discord bot setup; exits 1 when ok is false",
      formatter_class=help_formatter.CliHelpFormatter)
  return parser


def _validate_read_limit(limit: int) -> int:
  if not _READ_LIMIT_MIN <= limit <= _READ_LIMIT_MAX:
    common.exit_usage_error(f"--limit must be between {_READ_LIMIT_MIN} and {_READ_LIMIT_MAX}, got: {limit}")
  return limit


def _cmd_reply(args: argparse.Namespace) -> None:
  session_id = common.resolve_session_id(args.session)
  text = common.read_reply_text(args.file)
  result = common.post_internal_api("/api/internal/discord/reply", {"session_id": session_id, "text": text})
  print(json.dumps(result))


def _cmd_read(args: argparse.Namespace) -> None:
  session_id = common.resolve_session_id(args.session)
  limit = _validate_read_limit(args.limit)
  result = common.post_internal_api(
      "/api/internal/discord/read", {
          "session_id": session_id,
          "url": args.url,
          "limit": limit
      })
  print(json.dumps(result))


def _cmd_check() -> None:
  result = common.post_internal_api("/api/internal/discord/check", {})
  print(json.dumps(result))
  if not result.get("ok"):
    sys.exit(1)


def main() -> None:
  parser = _build_parser()
  args = parser.parse_args()
  if args.discord_command == "reply":
    _cmd_reply(args)
  elif args.discord_command == "read":
    _cmd_read(args)
  elif args.discord_command == "check":
    _cmd_check()
  else:
    parser.error(f"unknown discord command: {args.discord_command}")


if __name__ == "__main__":
  main()
