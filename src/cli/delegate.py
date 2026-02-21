"""CLI script for master CC to delegate tasks to worker agents.

Called by the master Claude Code instance via its run_command tool:

  python -m src.cli.delegate \
    --session SESSION_ID \
    --description "implement feature X"

The --repo flag defaults to the git root of the current working directory,
so the master agent automatically passes the correct repo when it runs
this command from inside the repo.
"""

import argparse
import json
import subprocess
import sys

import requests

from src.core.config import get_config


def _git_toplevel() -> str | None:
  """Return the git repo root for the current working directory, or None."""
  try:
    result = subprocess.run(
      ["git", "rev-parse", "--show-toplevel"],
      capture_output=True, text=True, timeout=5,
    )
    if result.returncode == 0:
      return result.stdout.strip()
  except Exception:
    pass
  return None


def main() -> None:
  parser = argparse.ArgumentParser(description="Delegate a task to a CharlieBot worker agent")
  parser.add_argument("--session", required=True, help="Session ID")
  parser.add_argument("--description", required=True, help="Task description")
  parser.add_argument("--repo", default=None, help="Repo path (default: git root of cwd)")
  args = parser.parse_args()

  repo_path = args.repo or _git_toplevel()

  cfg = get_config()
  port = cfg.server_port

  payload = {
    "session_id": args.session,
    "description": args.description,
    "repo_path": repo_path,
  }

  try:
    resp = requests.post(f"http://localhost:{port}/api/internal/delegate", json=payload, timeout=30)
    resp.raise_for_status()
    result = resp.json()
    print(json.dumps(result, indent=2))
  except requests.RequestException as e:
    print(json.dumps({"error": str(e)}), file=sys.stderr)
    sys.exit(1)


if __name__ == "__main__":
  main()
