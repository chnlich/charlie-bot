"""CLI verbs for session-level mutations, callable from any agent session.

  charliebot session create --name N [--backend B] [--group G]
  charliebot session create --parent P --profile manager --task-file FILE
  charliebot session tree [--root ID] [--include-archived] [--limit N] [--cursor C]
  charliebot session retry ID --run RUN_ID [--request-id ID]
  charliebot session send <target-id> (--message T | --file P)
  charliebot session dialog [--session ID]
  charliebot session resource-class ID {repair,agent}

``create`` builds session metadata only (no first message); with ``--parent``
it becomes the v2 task create: the task-file carries the ``task`` object
(goal/acceptance/context_refs/repo_path/base_branch/task_type/keep_worktree),
the server binds (parent, request_id) to one stable node, and a replayed
request returns the original product. A run-token agent organizes only its own
task: logical manager children under its own open task need no user
authorization, while worker children ride the runtime gate; any
parent other than the caller's own task is refused. ``tree`` pages the task
tree. ``retry`` creates the request-bound retry run of one recorded run.

``archive`` ends a task: operators archive its open subtree, and an agent may
archive its own manager task through its active Run. The optional result file
carries the archive request's JSON body (summary/result_refs/run_ids); an agent
must provide it, and its request closes after the Run succeeds. A duplicate
request id reuses its original result. ``cancel`` explicitly cancels an
open task with a reason — an operator cancels any open task, an agent running
inside a Run cancels only a direct child of its own task. A worker target has
its running work stopped and its delegation ended. The server route is
``POST /api/sessions/{id}/archive``. The user's own message to an archived node
restores it, and unarchiving is the sidebar's restore.

``acknowledge`` durably resolves exact task inputs the operator handled
out-of-band (the terminal-driven node's resolution step): operator scope,
exact input ids, idempotent replay; later arrivals keep blocking closure.

``send`` relays a message into the target session as an ``agent_message``
event (never a ``user`` event), so the runtime gate never counts it as a user message.
The server refuses a message over
MAX_AGENT_MESSAGE_WEIGHTED_CHARS weighted characters (CJK characters, CJK
punctuation, and fullwidth characters count 2) with exit code 2 and no
effect; long content belongs in a file the message points at. The caller
session resolves per ``resolve_session_id``.

``dialog`` prints one session's chat-visible messages as plain text, oldest
first, for piping into ``rg`` — the post-compaction search path the master
rules point at. It is read-only: it walks the same events pages the chat UI
does, starting from the bootstrap cursor, so an archived session (whose older
weeks live in ``data/archives/``) prints its complete history too. Tool
outputs and thinking stay out; each tool call renders one ``$`` line.

Authentication: with CHARLIEBOT_RUN_TOKEN set (an agent running inside a Run)
every request carries that token and nothing else — a rejection surfaces the
server's 401, never a silent operator-key fallback.
"""

import argparse
import json
import sys
import uuid

from src.infra import constants, help_formatter
from src.runtime.cli import common

# Exit code when the server refuses the send (over-limit message): mirrors
# argparse's usage-error exit code, matching schedule-trigger's 422 mapping.
EXIT_SEND_REFUSED = 2


def _build_parser() -> argparse.ArgumentParser:
  parser = argparse.ArgumentParser(
      description="CharlieBot session mutations", formatter_class=help_formatter.CliHelpFormatter)
  sub = parser.add_subparsers(dest="session_command", required=True)

  create = sub.add_parser(
      "create",
      help="Create a session (metadata only, no first message)",
      formatter_class=help_formatter.CliHelpFormatter)
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

  retry = sub.add_parser("retry", help="Create the retry run of one recorded run")
  retry.add_argument("session_id", help="Task id")
  retry.add_argument("--run", required=True, help="The run id being retried")
  retry.add_argument(
      "--request-id",
      default=None,
      help="Request id binding the retry run (defaults to a fresh UUID; replays return the same run)")

  archive = sub.add_parser("archive", help="Archive a task or its subtree")
  archive.add_argument("session_id", help="Task id")
  archive.add_argument(
      "--result-file",
      default=None,
      help="Path to the archive request's JSON body (summary, result_refs, run_ids; required for an agent)")
  archive.add_argument("--request-id", default=None, help="Request id binding the archive (defaults to a fresh UUID)")

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

  cancel = sub.add_parser("cancel", help="Cancel a task; stop runs and end delegation for workers")
  cancel.add_argument("session_id", help="Task id")
  cancel.add_argument("--reason", required=True, help="Cancellation reason")
  cancel.add_argument("--request-id", default=None, help="Request id binding the cancel (defaults to a fresh UUID)")

  send = sub.add_parser(
      "send",
      help="Relay a message to another session as an agent_message",
      formatter_class=help_formatter.CliHelpFormatter)
  send.add_argument("target", help="Target session id")
  source = send.add_mutually_exclusive_group(required=True)
  source.add_argument(
      "--message",
      default=None,
      help=(
          "Message text, at most "
          f"{constants.MAX_AGENT_MESSAGE_WEIGHTED_CHARS} weighted characters "
          "(CJK characters, CJK punctuation, and fullwidth characters count 2; "
          "long content belongs in a file)"))
  source.add_argument(
      "--file",
      default=None,
      help=(
          "Read the message text from this file; the same "
          f"{constants.MAX_AGENT_MESSAGE_WEIGHTED_CHARS} weighted-character "
          "limit applies to its content"))
  common.add_session_arg(send)

  dialog = sub.add_parser("dialog", help="Print one session's chat messages as plain text, oldest first (for rg)")
  common.add_session_arg(dialog)

  repair = sub.add_parser(
      "resource-class",
      help="Set a session's memory share: repair keeps it working while the agents' share is full (operator only)",
      formatter_class=help_formatter.CliHelpFormatter)
  repair.add_argument("session_id", help="Session id")
  repair.add_argument(
      "resource_class",
      choices=["repair", "agent"],
      help="repair moves this session and its descendants to the repair share; agent removes the mark")
  return parser


def _cmd_create(args: argparse.Namespace) -> None:
  payload: dict = {}
  if args.name is not None:
    payload["name"] = args.name
  if args.backend is not None:
    payload["backend"] = args.backend
  if args.parent is not None or args.profile is not None or args.task_file is not None:
    if args.parent is None or not args.profile:
      common.exit_usage_error("v2 task create requires both --parent and --profile")
    if args.task_file is not None:
      payload["task"] = json.loads(common.read_required_text_file("--task-file", args.task_file))
    payload["task_parent_id"] = args.parent
    payload["profile"] = args.profile
    payload["request_id"] = args.request_id or str(uuid.uuid4())
  result = common.post_internal_api("/api/sessions/", payload)
  if args.group is not None:
    result = common.post_internal_api(f"/api/sessions/{result['id']}/group", {"group": args.group})
  common.print_json(result, indent=2, file=sys.stdout)


def _cmd_tree(args: argparse.Namespace) -> None:
  params: dict = {"limit": args.limit, "include_archived": "true" if args.include_archived else "false"}
  if args.root is not None:
    params["parent_id"] = args.root
  if args.cursor is not None:
    params["cursor"] = args.cursor
  common.print_json(common.get_api("/api/sessions/tree", params), indent=2, file=sys.stdout)


def _cmd_retry(args: argparse.Namespace) -> None:
  payload = {"request_id": args.request_id or str(uuid.uuid4()), "run_id": args.run}
  common.print_json(
      common.post_internal_api(f"/api/sessions/{args.session_id}/retry", payload), indent=2, file=sys.stdout)


def _cmd_archive(args: argparse.Namespace) -> None:
  body = json.loads(common.read_required_text_file("--result-file",
                                                   args.result_file)) if args.result_file is not None else {}
  if not isinstance(body, dict):
    common.exit_usage_error("--result-file must carry the archive request's JSON object")
  body = dict(body)
  request_id = args.request_id or body.get("request_id") or str(uuid.uuid4())
  body["request_id"] = request_id
  result = common.post_internal_api(
      f"/api/sessions/{args.session_id}/archive",
      body,
      readback=lambda: common.find_local_task_close(args.session_id, request_id))
  common.print_json(result, indent=2, file=sys.stdout)


def _cmd_acknowledge(args: argparse.Namespace) -> None:
  input_ids = [part.strip() for part in args.input_ids.split(",") if part.strip()]
  if not input_ids:
    common.exit_usage_error("--input-ids must name at least one input event id")
  body = {
      "request_id": args.request_id or str(uuid.uuid4()),
      "input_ids": input_ids,
      "note": args.note,
  }
  common.print_json(
      common.post_internal_api(f"/api/sessions/{args.session_id}/task-inputs/acknowledge", body),
      indent=2,
      file=sys.stdout)


def _cmd_cancel(args: argparse.Namespace) -> None:
  request_id = args.request_id or str(uuid.uuid4())
  payload = {
      "request_id": request_id,
      "reason": args.reason,
  }
  result = common.post_internal_api(
      f"/api/sessions/{args.session_id}/cancel",
      payload,
      readback=lambda: common.find_local_task_close(args.session_id, request_id))
  common.print_json(result, indent=2, file=sys.stdout)
  unresolved = [item for item in result.get("stopped_runs", []) if item["outcome"] is None]
  if unresolved:
    run_ids = {item["run_id"] for item in unresolved}
    run_rows: dict[str, dict] = {}
    cursor = None
    while run_ids - run_rows.keys():
      params = {"limit": 500}
      if cursor is not None:
        params["cursor"] = cursor
      page = common.get_api(f"/api/sessions/{args.session_id}/runs", params)
      run_rows.update({row["id"]: row for row in page["items"] if row["id"] in run_ids})
      cursor = page["next_cursor"]
      if cursor is None:
        break
    for item in unresolved:
      print(f"run {item['run_id']} pid {run_rows[item['run_id']]['pid']} did not exit", file=sys.stderr)
    raise SystemExit(1)


def _cmd_send(args: argparse.Namespace) -> None:
  session_id = common.resolve_session_id(args.session)
  if args.message is not None:
    content = args.message
  elif args.file is not None:
    content = common.read_required_text_file("--file", args.file)
  else:
    common.exit_usage_error("one of --message or --file is required")
  result = common.post_internal_api(
      "/api/internal/session-message", {
          "session_id": session_id,
          "target_session_id": args.target,
          "content": content,
      },
      rejection_exit_codes={422: EXIT_SEND_REFUSED})
  common.print_json(result, indent=2, file=sys.stdout)


_DIALOG_PAGE_LIMIT = 200  # the events endpoint's clamp; the fewest requests a full history admits


def _tool_input_first_line(tool: dict) -> str:
  """One tool call's natural one-line form: Bash carries its command, every other
  tool a compact JSON rendering of the input; only the first line survives."""
  input_val = tool.get("input")
  if isinstance(input_val, dict):
    if tool.get("name") in ("Bash", "bash") and isinstance(input_val.get("command"), str):
      rendered = input_val["command"]
    else:
      rendered = json.dumps(input_val, ensure_ascii=False)
  elif isinstance(input_val, str):
    rendered = input_val
  else:
    rendered = ""
  return rendered.split("\n", 1)[0]


def _format_message_block(msg: dict) -> str | None:
  """One block per chat message with a body or at least one tool call; None when
  the message has neither (a turn divider prints nothing)."""
  content = msg.get("content")
  body = content if isinstance(content, str) else "" if content is None else json.dumps(content, ensure_ascii=False)
  tools = msg.get("tools") or []
  if not body and not tools:
    return None
  lines = [f"## {msg.get('role')} · {msg.get('timestamp') or ''} · event {msg.get('event_index')}"]
  if body:
    lines.append(body)
  lines.extend(f"$ {tool.get('name')} {_tool_input_first_line(tool)}" for tool in tools)
  return "\n".join(lines)


def _cmd_resource_class(args: argparse.Namespace) -> None:
  result = common.put_internal_api(
      f"/api/sessions/{args.session_id}/resource-class", {"resource_class": args.resource_class})
  common.print_json(result, indent=2, file=sys.stdout)


def _cmd_dialog(args: argparse.Namespace) -> None:
  session_id = common.resolve_session_id(args.session)
  # The bootstrap payload is the newest page the chat UI opens with; its
  # oldest_message_ordinal is the cursor backward paging starts from, in the
  # events endpoint's own cursor space (message ordinals for an ordinary
  # session, legacy event indices for an archived one).
  boot = common.get_api(f"/api/sessions/{session_id}/bootstrap")
  pages: list[list[dict]] = [boot["messages"]]
  cursor = boot["oldest_message_ordinal"]
  has_more = boot["has_more"]
  while has_more:
    page = common.get_api(f"/api/sessions/{session_id}/events", {"before": cursor, "limit": _DIALOG_PAGE_LIMIT})
    pages.append(page["messages"])
    cursor = page["next_before"]
    has_more = page["has_more"]
  blocks = [block for page in reversed(pages) for msg in page if (block := _format_message_block(msg)) is not None]
  if blocks:
    print("\n\n".join(blocks))


def main() -> None:
  parser = _build_parser()
  args = parser.parse_args()
  if args.session_command == "create":
    _cmd_create(args)
  elif args.session_command == "tree":
    _cmd_tree(args)
  elif args.session_command == "retry":
    _cmd_retry(args)
  elif args.session_command == "acknowledge":
    _cmd_acknowledge(args)
  elif args.session_command == "archive":
    _cmd_archive(args)
  elif args.session_command == "cancel":
    _cmd_cancel(args)
  elif args.session_command == "send":
    _cmd_send(args)
  elif args.session_command == "dialog":
    _cmd_dialog(args)
  elif args.session_command == "resource-class":
    _cmd_resource_class(args)
  else:
    parser.error(f"unknown session command: {args.session_command}")


if __name__ == "__main__":
  main()
