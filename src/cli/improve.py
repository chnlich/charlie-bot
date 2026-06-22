"""CLI script for master CC to start an iterative improvement loop.

Called by the master Claude Code instance via its run_command tool:

  # --session is optional; auto-derived from cwd in normal master use.
  charliebot improve \
    --repo /path/to/repo \
    --base-branch main \
    --iterations 3 \
    --goal-file /path/to/goal.md \
    --plan-file /path/to/plan.md

The CLI reads the goal file, optionally reads the plan file, and sends their
content in the API payload. It posts to the server-side /api/internal/improve
endpoint and returns immediately. The iteration loop runs as a background async
task on the server. Master CC will be notified via _trigger_master when the loop
completes.
"""

import argparse
import json
import sys
from pathlib import Path

from src.cli.common import post_internal_api, resolve_session_id


def _read_required_markdown_file(flag_name: str, file_path: str) -> str:
  """Read a required markdown file, exiting non-zero on a missing or empty file."""
  path = Path(file_path)
  if not path.is_file():
    print(json.dumps({"error": f"{flag_name} not found: {file_path}"}), file=sys.stderr)
    sys.exit(2)
  content = path.read_text()
  if not content.strip():
    print(json.dumps({"error": f"{flag_name} is empty: {file_path}"}), file=sys.stderr)
    sys.exit(2)
  return content


def _read_goal_file(goal_file: str) -> str:
  """Read the goal file, exiting non-zero on a missing or empty file."""
  return _read_required_markdown_file("--goal-file", goal_file)


def _read_plan_file(plan_file: str) -> str:
  """Read the optional plan file when provided, exiting non-zero if invalid."""
  return _read_required_markdown_file("--plan-file", plan_file)


def main() -> None:
  parser = argparse.ArgumentParser(description="Run an iterative improvement loop via CharlieBot workers")
  parser.add_argument("--session", required=False, default=None, help="Session ID (optional; auto-derived from cwd)")
  parser.add_argument("--repo", required=True, help="Path to the git repo workers should operate on")
  parser.add_argument("--iterations", type=int, default=3, help="Number of iterations to run")
  parser.add_argument(
      "--goal-file",
      dest="goal_file",
      required=True,
      help="Path to a file containing the improvement goal (read and sent as the goal payload)")
  parser.add_argument(
      "--plan-file",
      dest="plan_file",
      required=False,
      default=None,
      help="Optional path to a file containing the improvement plan (read and sent as the plan payload)")
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
  session_id = resolve_session_id(args.session)
  goal = _read_goal_file(args.goal_file)
  plan = _read_plan_file(args.plan_file) if args.plan_file is not None else None

  payload = {
      "session_id": session_id,
      "repo_path": args.repo,
      "base_branch": args.base_branch,
      "backend": args.backend,
      "iterations": args.iterations,
      "goal": goal,
      "work_branch": args.work_branch,
      "merge_back": args.merge_back,
  }
  if plan is not None:
    payload["plan"] = plan
  if args.backend is not None:
    payload["backend"] = args.backend

  result = post_internal_api("/api/internal/improve", payload)
  print(json.dumps(result, indent=2))


if __name__ == "__main__":
  main()
