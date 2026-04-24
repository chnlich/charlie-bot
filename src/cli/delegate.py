"""CLI script for master CC to delegate tasks to worker agents.

Called by the master Claude Code instance via its run_command tool:

  python -m src.cli.delegate \
    --session SESSION_ID \
    --repo /path/to/repo \
    --base-branch main \
    --description "implement feature X"
"""

import argparse
import json

from src.cli.common import post_internal_api


def main() -> None:
  parser = argparse.ArgumentParser(description="Delegate a task to a CharlieBot worker agent")
  parser.add_argument("--session", required=True, help="Session ID")
  parser.add_argument("--repo", required=True, help="Path to the git repo the worker should operate on")
  parser.add_argument("--description", required=True, help="Task description")
  parser.add_argument("--base-branch", required=True, help="Base branch for the worktree")
  parser.add_argument("--backend", default=None, help="Configured backend option id from ~/.charliebot/config.yaml")
  parser.add_argument("--context", default=None, help="Business context for reviewers")
  parser.add_argument(
      "--keep-worktree",
      action="store_true",
      default=False,
      help=("Leave the worker's worktree on disk after the worker exits AND after the reviewer merges. "
            "Use when the worker launches an external long-running process (e.g. a SLURM job) "
            "whose WorkDir points into the worktree."),
  )
  args = parser.parse_args()

  payload = {
      "session_id": args.session,
      "description": args.description,
      "base_branch": args.base_branch,
      "keep_worktree": args.keep_worktree,
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
