"""CLI script for master CC to schedule a delayed trigger.

Called by the master Claude Code instance via its run_command tool:

  python -m src.cli.schedule_trigger \
    --session SESSION_ID \
    --delay 3600 \
    --message 'Check PID 12345'

Optional: watch one or more PIDs. The trigger fires when ALL watched PIDs have
exited OR when --delay elapses (whichever comes first). When --watch-pid is
given, --delay is the max-wait upper bound.

  python -m src.cli.schedule_trigger \
    --session SESSION_ID \
    --delay 3600 \
    --watch-pid 12345 67890 \
    --message 'Training finished'
"""

import argparse
import json
import sys

import requests

from src.core.config import get_config
from src.core.timeouts import HTTP_INTERNAL_API_TIMEOUT


def main() -> None:
  parser = argparse.ArgumentParser(description="Schedule a delayed trigger for a CharlieBot session")
  parser.add_argument("--session", required=True, help="Session ID")
  parser.add_argument(
      "--delay",
      required=True,
      type=int,
      help="Delay in seconds before the trigger fires. When --watch-pid is set, this is the max-wait upper bound.",
  )
  parser.add_argument("--message", required=True, help="Message to send to the master CC when the trigger fires")
  parser.add_argument(
      "--watch-pid",
      nargs="+",
      type=int,
      metavar="PID",
      default=None,
      help="Optional PIDs to watch. Trigger fires when ALL listed PIDs exit OR --delay elapses, whichever first.",
  )
  args = parser.parse_args()

  cfg = get_config()

  payload: dict = {
      "session_id": args.session,
      "delay_seconds": args.delay,
      "message": args.message,
  }
  if args.watch_pid is not None:
    payload["watch_pids"] = args.watch_pid

  try:
    resp = requests.post(
        f"{cfg.server_base_url}/api/internal/schedule-trigger",
        json=payload,
        timeout=HTTP_INTERNAL_API_TIMEOUT,
        verify=False,
    )
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
