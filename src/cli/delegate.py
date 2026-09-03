"""CLI script for master CC to delegate tasks to worker agents.

Called by the master Claude Code instance via its run_command tool:

  # --session is optional; auto-derived from cwd in normal master use.
  charliebot delegate \
    --repo /path/to/repo \
    --base-branch main \
    --task-spec-file /path/to/task_spec.md \
    --reviewer-context-file /path/to/reviewer_context.md \
    --keep-worktree 0 \
    --task-type implement
"""

import argparse
import json
import sys

from src.cli.common import (
    add_session_arg,
    find_local_thread,
    post_internal_api,
    read_required_text_file,
    resolve_session_id,
    validate_repo_path,
    validate_task_spec_markdown,
)

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
  session backend; verify is routed to the first model_preference entry
  that differs from it. An explicit --backend replaces that routing for
  every task type, verify included.
"""


def main() -> None:
  parser = argparse.ArgumentParser(
      description="Delegate a task to a CharlieBot worker agent",
      epilog=DELEGATE_EPILOG,
      formatter_class=argparse.RawDescriptionHelpFormatter,
  )
  add_session_arg(parser)
  parser.add_argument(
      "--repo",
      required=False,
      help="Path to the git repo; required for implement/quick-edit/script-run, forbidden for verify")
  parser.add_argument(
      "--task-spec-file", dest="task_spec_file", required=True, help="Path to a structured Markdown task spec file")
  parser.add_argument(
      "--base-branch",
      required=False,
      help="Base branch for the worktree; required for implement/quick-edit/script-run, forbidden for verify")
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
      "--task-type",
      choices=["implement", "quick-edit", "script-run", "verify"],
      default="implement",
      help=(
          "Worker task profile. "
          "'implement' (default) = worker commits, reviewer rebases + ff-merges and pushes to the remote base branch. "
          "'quick-edit' = worker commits, no reviewer (use for trivial repo ops: cherry-picks, "
          "branch pushes, single-line/doc-only edits); master handles push/merge manually. "
          "'script-run' = worker uses worktree as an isolated sandbox to run scripts / submit jobs / "
          "query state; worker must NOT modify tracked files and must NOT commit. No reviewer, no merge. "
          "'verify' = repo-less read-only plan verifier; no worktree, reviewer, or merge."),
  )
  args = parser.parse_args()

  if args.task_type == "verify":
    if args.repo is not None:
      parser.error("--repo is forbidden when --task-type verify")
    if args.base_branch is not None:
      parser.error("--base-branch is forbidden when --task-type verify")
  else:
    if args.repo is None:
      parser.error(f"--repo is required when --task-type {args.task_type}")
    if args.base_branch is None:
      parser.error(f"--base-branch is required when --task-type {args.task_type}")
    validate_repo_path(parser, args.repo)

  session_id = resolve_session_id(args.session)
  task_spec = read_required_text_file("--task-spec-file", args.task_spec_file)
  validate_task_spec_markdown(task_spec)
  reviewer_context = None
  if args.reviewer_context_file is not None:
    reviewer_context = read_required_text_file("--reviewer-context-file", args.reviewer_context_file)

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

  def _readback() -> dict | None:
    # Sent-but-lost: this delegation's own thread is the proof the effect
    # landed. Matches the endpoint's response shape exactly.
    thread = find_local_thread(session_id, description=task_spec, task_type=args.task_type)
    if thread is None:
      return None
    return {"thread_id": thread["id"], "description": thread["description"]}

  result = post_internal_api("/api/internal/delegate", payload, readback=_readback)
  print("Worker spawned in the background; the completion summary arrives as an async wake-up.", file=sys.stderr)
  print(json.dumps(result, indent=2))


if __name__ == "__main__":
  main()
