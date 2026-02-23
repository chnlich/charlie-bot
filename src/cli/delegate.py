"""CLI script for master CC to delegate tasks to worker agents.

Called by the master Claude Code instance via its run_command tool:

  python -m src.cli.delegate \
    --session SESSION_ID \
    --repo /path/to/repo \
    --description "implement feature X"
"""

import argparse
import json
import sys

import requests

from src.core.config import get_config


def main() -> None:
  parser = argparse.ArgumentParser(description="Delegate a task to a CharlieBot worker agent")
  parser.add_argument("--session", required=True, help="Session ID")
  parser.add_argument("--repo", required=True, help="Path to the git repo the worker should operate on")
  parser.add_argument("--description", required=True, help="Task description")
  args = parser.parse_args()

  cfg = get_config()
  port = cfg.server_port

  payload = {
      "session_id": args.session,
      "description": args.description,
  }
  if args.repo is not None:
    payload["repo_path"] = args.repo

  try:
    resp = requests.post(f"https://localhost:{port}/api/internal/delegate", json=payload, timeout=30, verify=False)
    resp.raise_for_status()
    result = resp.json()
    print(json.dumps(result, indent=2))
  except requests.RequestException as e:
    print(json.dumps({"error": str(e)}), file=sys.stderr)
    sys.exit(1)


if __name__ == "__main__":
  main()
