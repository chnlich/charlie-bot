"""CLI script for master CC to delegate tasks to worker agents.

Called by the master Claude Code instance via its run_command tool:

  # --session is optional; auto-derived from cwd in normal master use.
  charliebot delegate \
    --repo /path/to/repo \
    --base-branch main \
    --description "implement feature X" \
    --keep-worktree 0 \
    --task-type implement
"""

import argparse
import json

from src.cli.common import post_internal_api, resolve_session_id


def main() -> None:
  parser = argparse.ArgumentParser(description="Delegate a task to a CharlieBot worker agent")
  parser.add_argument("--session", required=False, default=None, help="Session ID (optional; auto-derived from cwd)")
  parser.add_argument("--repo", required=True, help="Path to the git repo the worker should operate on")
  parser.add_argument("--description", required=True, help="Task description")
  parser.add_argument("--base-branch", required=True, help="Base branch for the worktree")
  parser.add_argument("--backend", default=None, help="Configured backend option id from ~/.charliebot/config.yaml")
  parser.add_argument("--context", default=None, help="Business context for reviewers")
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
      choices=["implement", "quick-edit", "script-run"],
      default="implement",
      help=(
          "Worker task profile. "
          "'implement' (default) = worker commits, reviewer rebases + ff-merges to base branch. "
          "'quick-edit' = worker commits, no reviewer (use for trivial repo ops: cherry-picks, "
          "branch pushes, single-line/doc-only edits); master handles push/merge manually. "
          "'script-run' = worker uses worktree as an isolated sandbox to run scripts / submit jobs / "
          "query state; worker must NOT modify tracked files and must NOT commit. No reviewer, no merge."),
  )
  args = parser.parse_args()
  session_id = resolve_session_id(args.session)

  payload = {
      "session_id": session_id,
      "description": args.description,
      "base_branch": args.base_branch,
      "keep_worktree": bool(args.keep_worktree),
      "task_type": args.task_type,
  }
  if args.backend is not None:
    payload["backend"] = args.backend
  if args.repo is not None:
    payload["repo_path"] = args.repo
  if args.context is not None:
    payload["context"] = args.context

  result = post_internal_api("/api/internal/delegate", payload)
  print(json.dumps(result, indent=2))


if __name__ == "__main__":
  main()
