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
import sys

import requests

from src.core.config import get_config
from src.core.timeouts import HTTP_INTERNAL_API_TIMEOUT


def main() -> None:
  parser = argparse.ArgumentParser(description="Delegate a task to a CharlieBot worker agent")
  parser.add_argument("--session", required=True, help="Session ID")
  parser.add_argument("--repo", required=True, help="Path to the git repo the worker should operate on")
  parser.add_argument("--description", required=True, help="Task description")
  parser.add_argument("--base-branch", required=True, help="Base branch for the worktree")
  parser.add_argument("--backend", default=None, help="Configured backend option id from ~/.charliebot/config.yaml")
  parser.add_argument("--context", default=None, help="Business context for reviewers")
  args = parser.parse_args()

  cfg = get_config()

  payload = {
      "session_id": args.session,
      "description": args.description,
      "base_branch": args.base_branch,
  }
  if args.backend is not None:
    payload["backend"] = args.backend
  if args.repo is not None:
    payload["repo_path"] = args.repo
  if args.context is not None:
    payload["context"] = args.context

  try:
    resp = requests.post(
        f"{cfg.server_base_url}/api/internal/delegate", json=payload, timeout=HTTP_INTERNAL_API_TIMEOUT, verify=False)
    resp.raise_for_status()
    result = resp.json()
    print(json.dumps(result, indent=2))
  except requests.RequestException as e:
    msg = str(e)
    if e.response is not None:
      try:
        msg = e.response.json()["detail"]
      except (ValueError, KeyError):
        pass
    print(json.dumps({"error": msg}), file=sys.stderr)
    sys.exit(1)


if __name__ == "__main__":
  main()
