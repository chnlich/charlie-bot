"""CLI script for master CC to start an iterative improvement loop.

Called by the master Claude Code instance via its run_command tool:

  python -m src.cli.improve \
    --session SESSION_ID \
    --repo /path/to/repo \
    --base-branch main \
    --iterations 3 \
    --goal 'optimize step time'

The CLI posts to the server-side /api/internal/improve endpoint and returns
immediately. The iteration loop runs as a background async task on the server.
Master CC will be notified via _trigger_master when the loop completes.
"""

import argparse
import json

from src.cli.common import post_internal_api


def main() -> None:
  parser = argparse.ArgumentParser(description="Run an iterative improvement loop via CharlieBot workers")
  parser.add_argument("--session", required=True, help="Session ID")
  parser.add_argument("--repo", required=True, help="Path to the git repo workers should operate on")
  parser.add_argument("--iterations", type=int, default=3, help="Number of iterations to run")
  parser.add_argument("--goal", required=True, help="Improvement goal")
  parser.add_argument("--base-branch", required=True, help="Base branch for iteration worktrees")
  parser.add_argument("--backend", default=None, help="Configured backend option id from ~/.charliebot/config.yaml")
  parser.add_argument(
      "--work-branch",
      "--branch-prefix",
      dest="work_branch",
      default=None,
      help="Single branch all iterations commit to (e.g. 'improve/optimize-step-time')")
  parser.add_argument(
      "--merge-back",
      action="store_true",
      default=False,
      help="Merge work_branch into base_branch after all iterations complete")
  args = parser.parse_args()

  payload = {
      "session_id": args.session,
      "repo_path": args.repo,
      "base_branch": args.base_branch,
      "backend": args.backend,
      "iterations": args.iterations,
      "goal": args.goal,
      "work_branch": args.work_branch,
      "merge_back": args.merge_back,
  }
  if args.backend is not None:
    payload["backend"] = args.backend

  result = post_internal_api("/api/internal/improve", payload)
  print(json.dumps(result, indent=2))


if __name__ == "__main__":
  main()
