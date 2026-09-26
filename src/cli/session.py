"""CLI verbs for session-level mutations, callable from any agent session.

  charliebot session create --name N [--backend B] [--group G]
  charliebot session create --parent P --profile manager --task-file FILE
  charliebot session tree [--root ID] [--include-archived] [--limit N] [--cursor C]
  charliebot session pause ID / resume ID
  charliebot session retry ID --run RUN_ID [--request-id ID]
  charliebot session send <target-id> (--message T | --file P)

``create`` builds session metadata only (no first message); with ``--parent``
it becomes the v2 task create: the task-file carries the ``task`` object
(goal/acceptance/context_refs/repo_path/base_branch/task_type/keep_worktree),
the server binds (parent, request_id) to one stable node, and a replayed
request returns the original product. A run-token agent organizes only its own
task: logical manager children under its own open task need no user
authorization, while worker children ride the implementation takeoff gate; any
parent other than the caller's own task is refused. ``tree`` pages the task tree. ``pause``/
``resume`` flip ``automation_paused`` (pausing never terminates a live run).
``retry`` creates the request-bound retry run of one recorded run.

``complete`` closes one task: the result file carries the complete request's
JSON body (summary/result_refs/run_ids); a duplicate request id replays the
original outcome, and an agent's own active Run may request its own manager's
closure (202 pending_run_finish). ``cancel`` explicitly cancels an open task
with a reason; ``reopen`` reopens one closed task and refuses closed
ancestors.

``acknowledge`` durably resolves exact task inputs the operator handled
out-of-band (the terminal-driven node's resolution step): operator scope,
exact input ids, idempotent replay; later arrivals keep blocking closure.

``send`` relays a message into the target session as an ``agent_message``
event (never a ``user`` event), so it neither mints nor revokes a takeoff
authorization window. The caller session resolves per ``resolve_session_id``.

Authentication: with CHARLIEBOT_RUN_TOKEN set (an agent running inside a Run)
every request carries that token and nothing else — a rejection surfaces the
server's 401, never a silent operator-key fallback.
"""

import argparse
import json
import uuid

from src.cli.common import (
    add_session_arg,
    exit_usage_error,
    get_api,
    patch_internal_api,
    post_internal_api,
    read_required_text_file,
    resolve_session_id,
)
from src.cli.help_formatter import CliHelpFormatter


def _build_parser() -> argparse.ArgumentParser:
  parser = argparse.ArgumentParser(description="CharlieBot session mutations", formatter_class=CliHelpFormatter)
  sub = parser.add_subparsers(dest="session_command", required=True)

  create = sub.add_parser(
      "create", help="Create a session (metadata only, no first message)", formatter_class=CliHelpFormatter)
  create.add_argument("--name", default=None, help="Session/task name (optional)")
  create.add_argument("--backend", default=None, help="Backend id (optional)")
  create.add_argument("--group", default=None, help="Group name to assign after creation (optional)")
  # ---- v2 task create ----
  create.add_argument("--parent", default=None, help="Parent task id (optional; v2 task create)")
  create.add_argument("--profile", default=None, choices=["manager", "worker"], help="Task profile (v2 task create)")
  create.add_argument(
      "--task-file",
      default=None,
      help="Path to the task object JSON (v2 task create; corresponds to the "
      "create request's 'task' field)")
  create.add_argument(
      "--request-id",
      default=None,
      help="Request id binding the stable node id (v2 task create; defaults to a fresh UUID)")

  tree = sub.add_parser("tree", help="Page the task tree")
  tree.add_argument("--root", default=None, help="Parent task id (default: the roots)")
  tree.add_argument("--include-archived", action="store_true", help="Include archived/collapsed rows")
  tree.add_argument("--limit", type=int, default=100, help="Page size (default 100)")
  tree.add_argument("--cursor", default=None, help="next_cursor from the previous page")

  pause = sub.add_parser("pause", help="Pause new automatic execution for one task")
  pause.add_argument("session_id", help="Task id")

  resume = sub.add_parser("resume", help="Resume automatic execution for one task")
  resume.add_argument("session_id", help="Task id")

  retry = sub.add_parser("retry", help="Create the retry run of one recorded run")
  retry.add_argument("session_id", help="Task id")
  retry.add_argument("--run", required=True, help="The run id being retried")
  retry.add_argument(
      "--request-id",
      default=None,
      help="Request id binding the retry run (defaults to a fresh UUID; replays return the same run)")

  complete = sub.add_parser("complete", help="Complete (close) one task")
  complete.add_argument("session_id", help="Task id")
  complete.add_argument(
      "--result-file",
      required=True,
      help="Path to the complete request's JSON body (summary, result_refs, run_ids; request_id "
      "optional and defaults to a fresh UUID)")
  complete.add_argument("--request-id", default=None, help="Request id binding the close (defaults to a fresh UUID)")

  ack = sub.add_parser(
      "acknowledge",
      help="Acknowledge exact task inputs the operator handled out-of-band "
      "(the terminal-driven node's resolution step)")
  ack.add_argument("session_id", help="Task id")
  ack.add_argument(
      "--input-ids",
      required=True,
      help="Comma-separated input event ids being resolved (each must currently be pending)")
  ack.add_argument("--note", default="", help="Optional note recorded with the acknowledgement")
  ack.add_argument(
      "--request-id",
      default=None,
      help="Request id binding the acknowledgement (defaults to a fresh UUID; replays return "
      "the original acknowledgement)")

  cancel = sub.add_parser("cancel", help="Explicitly cancel one open task")
  cancel.add_argument("session_id", help="Task id")
  cancel.add_argument("--reason", required=True, help="Cancellation reason")
  cancel.add_argument("--request-id", default=None, help="Request id binding the cancel (defaults to a fresh UUID)")

  reopen = sub.add_parser("reopen", help="Reopen one closed task")
  reopen.add_argument("session_id", help="Task id")
  reopen.add_argument("--reason", required=True, help="Reopen reason")
  reopen.add_argument("--request-id", default=None, help="Request id binding the reopen (defaults to a fresh UUID)")
  reopen.add_argument(
      "--closed-event", default=None, help="The task_closed event id to reopen (default: the latest close fact)")

  send = sub.add_parser(
      "send", help="Relay a message to another session as an agent_message", formatter_class=CliHelpFormatter)
  send.add_argument("target", help="Target session id")
  source = send.add_mutually_exclusive_group(required=True)
  source.add_argument("--message", default=None, help="Message text")
  source.add_argument("--file", default=None, help="Read the message text from this file")
  add_session_arg(send)
  return parser


def _cmd_create(args: argparse.Namespace) -> None:
  payload: dict = {}
  if args.name is not None:
    payload["name"] = args.name
  if args.backend is not None:
    payload["backend"] = args.backend
  if args.parent is not None or args.profile is not None or args.task_file is not None:
    if args.parent is None or args.profile is None:
      exit_usage_error("v2 task create requires both --parent and --profile")
    if args.task_file is not None:
      payload["task"] = json.loads(read_required_text_file("--task-file", args.task_file))
    payload["task_parent_id"] = args.parent
    payload["profile"] = args.profile
    payload["request_id"] = args.request_id or str(uuid.uuid4())
  result = post_internal_api("/api/sessions/", payload)
  if args.group is not None:
    result = post_internal_api(f"/api/sessions/{result['id']}/group", {"group": args.group})
  print(json.dumps(result, indent=2))


def _cmd_tree(args: argparse.Namespace) -> None:
  params: dict = {"limit": args.limit, "include_archived": "true" if args.include_archived else "false"}
  if args.root is not None:
    params["parent_id"] = args.root
  if args.cursor is not None:
    params["cursor"] = args.cursor
  print(json.dumps(get_api("/api/sessions/tree", params), indent=2))


def _set_paused(session_id: str, paused: bool) -> None:
  result = patch_internal_api(f"/api/sessions/{session_id}", {"automation_paused": paused})
  print(json.dumps({"id": result["id"], "automation_paused": result["automation_paused"]}, indent=2))


def _cmd_retry(args: argparse.Namespace) -> None:
  payload = {"request_id": args.request_id or str(uuid.uuid4()), "run_id": args.run}
  print(json.dumps(post_internal_api(f"/api/sessions/{args.session_id}/retry", payload), indent=2))


def _cmd_complete(args: argparse.Namespace) -> None:
  body = json.loads(read_required_text_file("--result-file", args.result_file))
  if not isinstance(body, dict):
    exit_usage_error("--result-file must carry the complete request's JSON object")
  body = dict(body)
  body["request_id"] = args.request_id or body.get("request_id") or str(uuid.uuid4())
  print(json.dumps(post_internal_api(f"/api/sessions/{args.session_id}/complete", body), indent=2))


def _cmd_acknowledge(args: argparse.Namespace) -> None:
  input_ids = [part.strip() for part in args.input_ids.split(",") if part.strip()]
  if not input_ids:
    exit_usage_error("--input-ids must name at least one input event id")
  body = {
      "request_id": args.request_id or str(uuid.uuid4()),
      "input_ids": input_ids,
      "note": args.note,
  }
  print(json.dumps(post_internal_api(f"/api/sessions/{args.session_id}/task-inputs/acknowledge", body), indent=2))


def _cmd_cancel(args: argparse.Namespace) -> None:
  payload = {
      "request_id": args.request_id or str(uuid.uuid4()),
      "reason": args.reason,
  }
  print(json.dumps(post_internal_api(f"/api/sessions/{args.session_id}/cancel", payload), indent=2))


def _cmd_reopen(args: argparse.Namespace) -> None:
  payload = {
      "request_id": args.request_id or str(uuid.uuid4()),
      "reason": args.reason,
  }
  if args.closed_event is not None:
    payload["closed_event_id"] = args.closed_event
  print(json.dumps(post_internal_api(f"/api/sessions/{args.session_id}/reopen", payload), indent=2))


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
  elif args.session_command == "tree":
    _cmd_tree(args)
  elif args.session_command == "pause":
    _set_paused(args.session_id, paused=True)
  elif args.session_command == "resume":
    _set_paused(args.session_id, paused=False)
  elif args.session_command == "retry":
    _cmd_retry(args)
  elif args.session_command == "acknowledge":
    _cmd_acknowledge(args)
  elif args.session_command == "complete":
    _cmd_complete(args)
  elif args.session_command == "cancel":
    _cmd_cancel(args)
  elif args.session_command == "reopen":
    _cmd_reopen(args)
  elif args.session_command == "send":
    _cmd_send(args)
  else:
    parser.error(f"unknown session command: {args.session_command}")


if __name__ == "__main__":
  main()
