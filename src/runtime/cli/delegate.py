"""CLI script for master CC to delegate tasks to worker agents.

Called by the master Claude Code instance as a shell command (session
identity resolves per ``common.resolve_session_id``):

  charliebot delegate \
    --repo /path/to/repo \
    --base-branch main \
    --task-spec-file /path/to/task_spec.md \
    --keep-worktree 0 \
    --task-type implement

A repo-less delegation omits --repo and --base-branch together; the worker
works from its Run directory on the host paths the task spec names:

  charliebot delegate --task-spec-file /path/to/task_spec.md --keep-worktree 0
"""

import argparse
import sys

from src.infra import help_formatter
from src.runtime.cli import common

DELEGATE_EPILOG = """\
Task spec (--task-spec-file), six sections:

  ## Goal - one concise deliverable.
  ## Source Files - absolute paths, or `- (none)`.
  ## Required Behavior - the executable contract, state-machine semantics, and boundary rules.
  ## Acceptance Tests - focused tests or verification commands.
  ## Reviewer Checklist - concrete checks beyond "tests passed".
  ## Out of Scope - things the worker must not change.

  Specs must not forbid test edits: tests asserting removed behavior get updated or deleted with it.

  Spec precision follows the task kind: an exploratory or judgment-heavy task (a cleanup whose
  extent is a judgment call, a review, a comment or doc rewrite) states the goal, the reason in
  the requester's words, the judging standard, the invariants, and the scope, and stops there -
  an enumerated change list caps the result at what the master already found. A fixed mechanical
  contract keeps its executable contract and acceptance assertions: there the precision is the
  deliverable.

  --repo and --base-branch: give both, or omit both for a repo-less implement, quick-edit, or
  script-run task. verify forbids both.
  --keep-worktree 1 keeps the worktree after the worker exits and the reviewer merges; pass it when
  the worker launches a long-running process, e.g. a SLURM job, whose WorkDir lives in the
  worktree. 0 cleans up.
  --request-id: replay id; the same id returns the original child. Default: derived from the
  request content, so an identical spec re-delegated from the same session returns the existing
  child; pass an explicit id for intentional same-spec siblings.

Task types (--task-type):

  implement (default): worker commits; a reviewer rebases, ff-merges, pushes to the remote base branch.
  quick-edit: worker commits; no reviewer; the master merges and pushes; for trivial repo ops (cherry-picks, branch pushes, single-line or doc-only edits).
  script-run: the worktree (repo-less: the Run directory) is a sandbox to run scripts, submit jobs, or query state; the worker must not modify tracked files or commit; no reviewer, no merge.
  verify: a repo-less read-only plan verifier; no worktree, reviewer, or merge.

Backend selection (--backend):

  Pass --backend only when the user or the line's goal names a backend. Omitted: implement,
  quick-edit, and script-run take the next backends.pool member that is not cooling, or inherit
  the session backend when the pool is empty; a worker out of quota continues on the next member
  in the same worktree. Verify routes to the next member that differs from the session backend.
  An explicit --backend replaces that routing for every task type, verify included, and is never
  failed over. The resolved backend runs every work Run of the worker session.
"""


def main() -> None:
  parser = argparse.ArgumentParser(
      description="Delegate a task to a CharlieBot worker agent",
      epilog=DELEGATE_EPILOG,
      formatter_class=help_formatter.CliRawDescriptionHelpFormatter,
  )
  common.add_session_arg(parser)
  parser.add_argument("--repo", required=False, help="Git repo path (rules in the epilog)")
  parser.add_argument(
      "--task-spec-file", dest="task_spec_file", required=True, help="Markdown task spec (format in the epilog)")
  parser.add_argument("--base-branch", required=False, help="Worktree base branch (rules in the epilog)")
  parser.add_argument(
      "--backend", default=None, help="Backend option id from $CHARLIEBOT_HOME/config.yaml (rules in the epilog)")
  parser.add_argument(
      "--keep-worktree",
      required=True,
      type=int,
      choices=[0, 1],
      help="0 or 1 (rule in the epilog)",
  )
  parser.add_argument(
      "--request-id", dest="request_id", default=None, help="Replay id (v2 task-tree sessions; rule in the epilog)")
  parser.add_argument(
      "--task-type",
      choices=["implement", "quick-edit", "script-run", "verify"],
      default="implement",
      help="Task profile (rules in the epilog)",
  )
  args, unknown_args = parser.parse_known_args()
  # The removed reviewer context file option gets an error that names its replacement; every other
  # unknown argument errors the way parse_args itself would.
  if any(arg.startswith("--reviewer-") for arg in unknown_args):
    parser.error(
        "the reviewer context file option was removed: put reviewer instructions under "
        "the task spec's ## Reviewer Checklist")
  if unknown_args:
    parser.error(f"unrecognized arguments: {' '.join(unknown_args)}")

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
              "reviewer_context_file": None,
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
  if args.request_id is not None:
    payload["request_id"] = args.request_id

  def _readback() -> dict | None:
    # Sent-but-lost: this delegation's task child and stable Run are the proof
    # the effect landed.
    return common.find_local_task_child(
        session_id, description=task_spec, task_type=args.task_type, request_id=args.request_id)

  result = common.post_internal_api("/api/internal/delegate", payload, readback=_readback)
  print("Worker spawned in the background; the completion summary arrives as an async wake-up.", file=sys.stderr)
  # Callers read only these two ids. The other server keys repeat the caller's own spec and ids, so
  # they stay off stdout.
  common.print_json({"session_id": result["session_id"], "run_id": result["run_id"]}, indent=2, file=sys.stdout)


if __name__ == "__main__":
  main()
