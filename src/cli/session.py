"""CLI verbs for session-level mutations, callable from any agent session.

  charliebot session create --name N [--backend B] [--group G] [--role R]
  charliebot session send <target-id> (--message T | --file P)

``create`` builds session metadata only (no first message); with ``--group`` a
second call assigns the group. ``send`` relays a message into the target
session as an ``agent_message`` event (never a ``user`` event), so it neither
mints nor revokes a takeoff authorization window. The caller session is
derived from cwd per the usual CLI convention.
"""

import argparse
import json

from src.cli.common import (
    exit_usage_error,
    post_internal_api,
    read_required_text_file,
    resolve_session_id,
)


def _build_parser() -> argparse.ArgumentParser:
  parser = argparse.ArgumentParser(description="CharlieBot session mutations")
  sub = parser.add_subparsers(dest="session_command", required=True)

  create = sub.add_parser("create", help="Create a session (metadata only, no first message)")
  create.add_argument("--name", required=True, help="Session name")
  create.add_argument("--backend", default=None, help="Backend id (optional)")
  create.add_argument("--group", default=None, help="Group name to assign after creation (optional)")
  create.add_argument("--role", default=None, help="Session role (optional)")

  send = sub.add_parser("send", help="Relay a message to another session as an agent_message")
  send.add_argument("target", help="Target session id")
  source = send.add_mutually_exclusive_group(required=True)
  source.add_argument("--message", default=None, help="Message text")
  source.add_argument("--file", default=None, help="Read the message text from this file")
  send.add_argument(
      "--session", required=False, default=None, help="Caller session id (optional; auto-derived from cwd)")
  return parser


def _cmd_create(args: argparse.Namespace) -> None:
  payload: dict = {"name": args.name}
  if args.backend is not None:
    payload["backend"] = args.backend
  if args.role is not None:
    payload["role"] = args.role
  result = post_internal_api("/api/sessions/", payload)
  if args.group is not None:
    result = post_internal_api(f"/api/sessions/{result['id']}/group", {"group": args.group})
  print(json.dumps(result, indent=2))


def _cmd_send(args: argparse.Namespace) -> None:
  session_id = resolve_session_id(args.session)
  if args.message is not None:
    content = args.message
  elif args.file is not None:
    content = read_required_text_file("--file", args.file)
  else:
    exit_usage_error("one of --message or --file is required")
  result = post_internal_api(
      "/api/internal/session-message", {
          "session_id": session_id,
          "target_session_id": args.target,
          "content": content,
      })
  print(json.dumps(result, indent=2))


def main() -> None:
  parser = _build_parser()
  args = parser.parse_args()
  if args.session_command == "create":
    _cmd_create(args)
  elif args.session_command == "send":
    _cmd_send(args)
  else:
    parser.error(f"unknown session command: {args.session_command}")


if __name__ == "__main__":
  main()
