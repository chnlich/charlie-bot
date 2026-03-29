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
import sys

import requests

from src.core.config import get_config


def main() -> None:
  parser = argparse.ArgumentParser(description="Run an iterative improvement loop via CharlieBot workers")
  parser.add_argument("--session", required=True, help="Session ID")
  parser.add_argument("--repo", required=True, help="Path to the git repo workers should operate on")
  parser.add_argument("--iterations", type=int, default=3, help="Number of iterations to run")
  parser.add_argument("--goal", required=True, help="Improvement goal")
  parser.add_argument("--base-branch", required=True, help="Base branch for iteration worktrees")
  parser.add_argument("--branch-prefix", default=None, help="Branch prefix for iteration branches (e.g. 'improve/perf')")
  args = parser.parse_args()

  cfg = get_config()

  payload = {
      "session_id": args.session,
      "repo_path": args.repo,
      "base_branch": args.base_branch,
      "iterations": args.iterations,
      "goal": args.goal,
      "branch_prefix": args.branch_prefix,
  }

  try:
    resp = requests.post(f"{cfg.server_base_url}/api/internal/improve", json=payload, timeout=30, verify=False)
    resp.raise_for_status()
    result = resp.json()
    print(json.dumps(result, indent=2))
  except requests.RequestException as e:
    print(json.dumps({"error": str(e)}), file=sys.stderr)
    sys.exit(1)


if __name__ == "__main__":
  main()
