"""CLI script for master CC to delegate tasks to worker agents.

Called by the master Claude Code instance via its run_command tool:

  # --session is optional; auto-derived from cwd when run inside a CharlieBot session dir.
  python -m src.cli.delegate \
    --repo /path/to/repo \
    --base-branch main \
    --description "implement feature X" \
    --keep-worktree 0 \
    --require-review 1
"""

import argparse
import json

from src.cli.common import post_internal_api, resolve_session_id


def main() -> None:
  parser = argparse.ArgumentParser(description="Delegate a task to a CharlieBot worker agent")
  parser.add_argument(
      "--session",
      required=False,
      default=None,
      help="Session ID (optional; auto-derived from cwd when run inside a CharlieBot session dir)")
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
      "--require-review",
      type=int,
      choices=[0, 1],
      default=1,
      help=(
          "0 = skip auto-spawn reviewer (use for trivial repo ops: cherry-picks, branch pushes, "
          "single-line edits, doc-only changes); 1 = default reviewer flow."),
  )
  args = parser.parse_args()
  session_id = resolve_session_id(args.session)

  payload = {
      "session_id": session_id,
      "description": args.description,
      "base_branch": args.base_branch,
      "keep_worktree": bool(args.keep_worktree),
      "require_review": bool(args.require_review),
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
