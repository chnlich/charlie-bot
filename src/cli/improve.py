"""CLI script for master CC to start an iterative improvement loop.

Called by the master Claude Code instance via its run_command tool:

  # --session is optional; the server-written CHARLIEBOT_SESSION_ID supplies it
  # in normal master use (see ``resolve_session_id``).
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

from src.cli.common import (
    add_session_arg,
    find_local_thread,
    post_internal_api,
    read_required_text_file,
    resolve_session_id,
    validate_repo_path,
)
from src.core.config import get_config


def _read_goal_file(goal_file: str) -> str:
  """Read the goal file, exiting non-zero on a missing or empty file."""
  return read_required_text_file("--goal-file", goal_file)


def _read_plan_file(plan_file: str) -> str:
  """Read the optional plan file when provided, exiting non-zero if invalid."""
  return read_required_text_file("--plan-file", plan_file)


IMPROVE_EPILOG = """\
The CLI creates a live goal at loops/{loop_id}/goal.md. Each
iteration re-reads it, so editing the file steers the remaining
iterations. The launch response includes loop_id and goal_path.

Runtime authorization (takeoff gate) is derived from the chat
event log; see skills/plan-approval/SKILL.md for the full contract.

Invalid iterations (a missing, malformed, or zero-commit-no-verdict
report) are quarantined from loop context and still consume an
iteration slot.
"""


def main() -> None:
  parser = argparse.ArgumentParser(
      description="Run an iterative improvement loop via CharlieBot workers",
      epilog=IMPROVE_EPILOG,
      formatter_class=argparse.RawDescriptionHelpFormatter,
  )
  add_session_arg(parser)
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
  validate_repo_path(parser, args.repo)
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

  def _readback() -> dict | None:
    # Sent-but-lost: the loop's live goal file plus an iteration-1 thread that
    # embeds this goal together prove the launch landed. Returns the endpoint's
    # response shape so steering output stays identical.
    cfg = get_config()
    loops_dir = cfg.sessions_dir / session_id / "loops"
    if not loops_dir.is_dir():
      return None
    candidates = []
    for loop_dir in loops_dir.iterdir():
      if not loop_dir.name.isdigit():
        continue
      try:
        if (loop_dir / "goal.md").read_text(encoding="utf-8") == goal:
          candidates.append(int(loop_dir.name))
      except OSError:
        continue
    if not candidates:
      return None
    thread = find_local_thread(
        session_id,
        description=f"Goal: {goal}",
        task_type="implement",
        description_match="contains",
    )
    if thread is None:
      return None
    loop_id = max(candidates)
    response = {
        "status": "started",
        "session_id": session_id,
        "iterations": args.iterations,
        "loop_id": loop_id,
        "goal_path": str(loops_dir / str(loop_id) / "goal.md"),
    }
    if plan is not None:
      response["plan_path"] = str(loops_dir / str(loop_id) / "plan.md")
    return response

  result = post_internal_api("/api/internal/improve", payload, readback=_readback)
  print(json.dumps(result, indent=2))


if __name__ == "__main__":
  main()
