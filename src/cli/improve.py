"""CLI script for master CC to run iterative improvement loops.

Called by the master Claude Code instance via its run_command tool:

  python -m src.cli.improve \
    --session SESSION_ID \
    --repo /path/to/repo \
    --iterations 3 \
    --goal 'optimize step time'
"""

import argparse
import json
import sys
import time

import requests

from src.core.config import get_config


def main() -> None:
  parser = argparse.ArgumentParser(description="Run an iterative improvement loop via CharlieBot workers")
  parser.add_argument("--session", required=True, help="Session ID")
  parser.add_argument("--repo", required=True, help="Path to the git repo workers should operate on")
  parser.add_argument("--iterations", type=int, default=3, help="Number of iterations to run")
  parser.add_argument("--goal", required=True, help="Improvement goal")
  args = parser.parse_args()

  cfg = get_config()
  port = cfg.server_port
  base_url = f"https://localhost:{port}"
  state_path = cfg.sessions_dir / args.session / "improve_state.json"

  previous_summaries: list[str] = []

  for i in range(1, args.iterations + 1):
    # Check if stopped
    if state_path.exists():
      try:
        state_data = json.loads(state_path.read_text())
        if state_data.get("status") == "stopped":
          print(f"Improve loop stopped by user at iteration {i}.")
          break
      except (json.JSONDecodeError, OSError):
        pass

    # Build iteration description
    desc_parts = [f"Iterative improvement — iteration {i}/{args.iterations}", f"Goal: {args.goal}"]
    if previous_summaries:
      desc_parts.append("Previous iterations:")
      for idx, summary in enumerate(previous_summaries, 1):
        desc_parts.append(f"  Iteration {idx}: {summary}")
    description = "\n".join(desc_parts)

    # Delegate to a worker
    payload = {
        "session_id": args.session,
        "repo_path": args.repo,
        "description": description,
        "require_review": False,
    }
    try:
      resp = requests.post(f"{base_url}/api/internal/delegate", json=payload, timeout=30, verify=False)
      resp.raise_for_status()
      result = resp.json()
      thread_id = result.get("thread_id")
    except requests.RequestException as e:
      print(json.dumps({"error": f"Failed to delegate iteration {i}: {e}"}), file=sys.stderr)
      sys.exit(1)

    if not thread_id:
      print(json.dumps({"error": f"No thread_id returned for iteration {i}"}), file=sys.stderr)
      sys.exit(1)

    # Poll until thread completes
    while True:
      time.sleep(5)
      try:
        resp = requests.get(
            f"{base_url}/api/threads/{args.session}/threads/{thread_id}",
            timeout=10,
            verify=False,
        )
        resp.raise_for_status()
        thread_data = resp.json()
        status = thread_data.get("status")
        if status in ("completed", "failed", "cancelled"):
          break
      except requests.RequestException:
        continue

    # Extract summary from thread events
    summary = ""
    try:
      resp = requests.get(
          f"{base_url}/api/threads/{args.session}/threads/{thread_id}/events",
          timeout=10,
          verify=False,
      )
      resp.raise_for_status()
      events = resp.json()
      # Find last assistant message
      for event in reversed(events):
        if event.get("type") == "assistant" and event.get("content"):
          summary = event["content"][:500]
          break
    except requests.RequestException:
      summary = f"Iteration {i} completed (could not retrieve summary)."

    previous_summaries.append(summary)
    print(f"Iteration {i}/{args.iterations}: {summary[:200]}")

  # Print final JSON summary
  print(json.dumps({
      "type": "improve_completed",
      "goal": args.goal,
      "iterations_completed": len(previous_summaries),
      "summaries": previous_summaries,
  }, indent=2))


if __name__ == "__main__":
  main()
