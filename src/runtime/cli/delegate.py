"""CLI script for master CC to delegate tasks to worker agents.

Called by the master Claude Code instance as a shell command (session
identity resolves per ``common.resolve_session_id``):

  charliebot delegate \
    --repo /path/to/repo \
    --base-branch main \
    --task-spec-file /path/to/task_spec.md \
    --reviewer-context-file /path/to/reviewer_context.md \
    --keep-worktree 0 \
    --task-type implement

A repo-less delegation omits --repo and --base-branch together; the worker
works from its Run directory on the host paths the task spec names:

  charliebot delegate --task-spec-file /path/to/task_spec.md --keep-worktree 0
"""

import argparse
import json
import sys

from src.infra import help_formatter
from src.runtime.cli import common

DELEGATE_EPILOG = """\
Task spec format (--task-spec-file):

  ## Goal
  One concise deliverable.
  ## Source Files
  - <absolute-source-path>
  ## Required Behavior
  Executable contract, state-machine semantics, and boundary rules.
  ## Acceptance Tests
  Focused tests or verification commands.
  ## Reviewer Checklist
  Concrete checks beyond "tests passed".
  ## Out of Scope
  Things the worker must not change.

  Spec precision follows the task kind. An exploratory or
  judgment-heavy task (a cleanup whose extent is a judgment call, a
  review, a comment or doc rewrite) states the goal, the reason it
  matters in the requester's own words, the standard the result is
  judged against, the invariants to keep, and what is out of scope,
  and stops there: an enumerated change list becomes the worker's
  whole scope and caps the result at what the master already found.
  A task with a fixed mechanical contract keeps its executable
  contract and acceptance assertions: there the precision is the
  deliverable.

  Source Files entries: absolute paths or `- (none)`.
  Task specs must not forbid test edits: updating affected tests is
  part of the change — tests asserting removed behavior get updated
  or deleted with it.
  Runtime authorization (takeoff gate) is derived from the chat
  event log; see skills/plan-approval/SKILL.md for the full contract.

Backend selection (--backend):

  Omit --backend unless the user explicitly named a backend for this
  delegation. Omitted: implement / quick-edit / script-run inherit the
  session backend;   verify is routed to the first backends.preference entry
  that differs from it. An explicit --backend replaces that routing for
  every task type, verify included.
"""


def main() -> None:
  parser = argparse.ArgumentParser(
      description="Delegate a task to a CharlieBot worker agent",
      epilog=DELEGATE_EPILOG,
      formatter_class=help_formatter.CliRawDescriptionHelpFormatter,
  )
  common.add_session_arg(parser)
  parser.add_argument(
      "--repo",
      required=False,
      help=(
          "Path to the git repo; optional for implement/quick-edit/script-run (omit together "
          "with --base-branch for a repo-less task), forbidden for verify"))
  parser.add_argument(
      "--task-spec-file", dest="task_spec_file", required=True, help="Path to a structured Markdown task spec file")
  parser.add_argument(
      "--base-branch",
      required=False,
      help=(
          "Base branch for the worktree; optional for implement/quick-edit/script-run (omit together "
          "with --repo for a repo-less task), forbidden for verify"))
  parser.add_argument(
      "--backend",
      default=None,
      help=(
          "Configured backend option id from ~/.charliebot/config.yaml; omit unless the user "
          "explicitly named a backend for this delegation (see epilog)"))
  parser.add_argument(
      "--reviewer-context-file",
      dest="reviewer_context_file",
      default=None,
      help="Optional path to reviewer-only context")
  parser.add_argument(
      "--keep-worktree",
      required=True,
      type=int,
      choices=[0, 1],
      help=(
          "1 = keep worktree on disk after worker exits AND after reviewer merges "
          "(use when the worker launches an external long-running process, e.g. a SLURM job, "
          "whose WorkDir lives in the worktree); "
          "0 = default cleanup behavior."),
  )
  parser.add_argument(
      "--request-id",
      dest="request_id",
      default=None,
      help=(
          "Stable operation id for the delegation (v2 task-tree sessions). A replayed "
          "create/delegate with the same request-id returns the original child instead of a "
          "second process. Omitted: a stable id is derived from the request content, so an "
          "identical spec re-delegated from the same session returns the existing child — "
          "pass an explicit id for intentional same-spec siblings."))
  parser.add_argument(
      "--task-type",
      choices=["implement", "quick-edit", "script-run", "verify"],
      default="implement",
      help=(
          "Worker task profile. "
          "'implement' (default) = worker commits, reviewer rebases + ff-merges and pushes to the remote base branch. "
          "'quick-edit' = worker commits, no reviewer (use for trivial repo ops: cherry-picks, "
          "branch pushes, single-line/doc-only edits); master handles push/merge manually. "
          "'script-run' = worker uses the worktree (or, repo-less, the Run directory) as an isolated "
          "sandbox to run scripts / submit jobs / query state; worker must NOT modify tracked files and "
          "must NOT commit. No reviewer, no merge. "
          "'verify' = repo-less read-only plan verifier; no worktree, reviewer, or merge."),
  )
  args = parser.parse_args()

  if args.task_type == "verify":
    if args.repo is not None:
      parser.error("--repo is forbidden when --task-type verify")
    if args.base_branch is not None:
      parser.error("--base-branch is forbidden when --task-type verify")
  else:
    # implement/quick-edit/script-run carry a repo, or neither flag: a
    # repo-less Run works from its Run directory, and one flag without the
    # other names a worktree that cannot exist.
    if (args.repo is None) != (args.base_branch is None):
      parser.error(
          f"--repo and --base-branch are given together for a repo task or omitted together "
          f"for a repo-less one; exactly one was given for --task-type {args.task_type}")
    if args.repo is not None:
      common.validate_repo_path(parser, args.repo)

  session_id = common.resolve_session_id(args.session)
  task_spec = common.read_required_text_file("--task-spec-file", args.task_spec_file)
  common.validate_task_spec_markdown(task_spec)
  reviewer_context = None
  if args.reviewer_context_file is not None:
    reviewer_context = common.read_required_text_file("--reviewer-context-file", args.reviewer_context_file)

  payload = {
      "session_id": session_id,
      "description": task_spec,
      "keep_worktree": bool(args.keep_worktree),
      "task_type": args.task_type,
      "delegate_invocation":
          {
              "task_type": args.task_type,
              "repo_path": args.repo,
              "base_branch": args.base_branch,
              "task_spec_file": args.task_spec_file,
              "reviewer_context_file": args.reviewer_context_file,
              "keep_worktree": bool(args.keep_worktree),
              "backend": args.backend,
          },
  }
  if args.base_branch is not None:
    payload["base_branch"] = args.base_branch
  if args.backend is not None:
    payload["backend"] = args.backend
  if args.repo is not None:
    payload["repo_path"] = args.repo
  if reviewer_context is not None:
    payload["context"] = reviewer_context
  if args.request_id is not None:
    payload["request_id"] = args.request_id

  def _readback() -> dict | None:
    # Sent-but-lost: this delegation's own product is the proof the effect
    # landed. A v2 task-tree child (a worker task under this session with the
    # same spec) returns the new {session_id, parent_session_id, run_id,
    # thread_id} contract; a v1 session keeps its legacy thread shape.
    child = common.find_local_task_child(
        session_id, description=task_spec, task_type=args.task_type, request_id=args.request_id)
    if child is not None:
      return child
    thread = common.find_local_thread(session_id, description=task_spec, task_type=args.task_type)
    if thread is None:
      return None
    return {"thread_id": thread["id"], "description": thread["description"]}

  result = common.post_internal_api("/api/internal/delegate", payload, readback=_readback)
  print("Worker spawned in the background; the completion summary arrives as an async wake-up.", file=sys.stderr)
  print(json.dumps(result, indent=2))


if __name__ == "__main__":
  main()
