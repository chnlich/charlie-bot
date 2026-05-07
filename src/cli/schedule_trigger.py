"""CLI script for master CC to schedule a delayed trigger.

Called by the master Claude Code instance via its run_command tool. ``--session``
is optional; auto-derived from cwd when run inside a CharlieBot session dir.

  python -m src.cli.schedule_trigger \
    --max-wait 3600 \
    --message 'Check PID 12345'

Optional: watch one or more PIDs. The trigger fires when ALL watched PIDs have
exited OR when --max-wait elapses (whichever comes first). Each --watch-pid
value is either a local PID (e.g. ``12345``) or a remote PID of the form
``host:pid`` (e.g. ``neptune:12345``). All values in a single trigger must be
either all local or all remote — mixing is rejected.

  python -m src.cli.schedule_trigger \
    --max-wait 3600 \
    --watch-pid 12345 67890 \
    --message 'Training finished'

  python -m src.cli.schedule_trigger \
    --max-wait 3600 \
    --watch-pid neptune:12345 noire:67890 \
    --message 'Remote training finished'
"""

import argparse
import json
import sys

import requests

from src.cli.common import resolve_session_id
from src.core.config import get_config
from src.core.timeouts import HTTP_INTERNAL_API_TIMEOUT


# Exit code returned when remote-PID verify-on-create rejects the trigger.
EXIT_VERIFY_REJECTED = 2


def _parse_watch_target(raw: str) -> dict:
  """Parse a single --watch-pid value into a dict {'host': str|None, 'pid': int}."""
  if ":" in raw:
    host, _, pid_str = raw.partition(":")
    if not host:
      raise argparse.ArgumentTypeError(f"--watch-pid host must be non-empty (got {raw!r})")
    try:
      pid = int(pid_str)
    except ValueError as e:
      raise argparse.ArgumentTypeError(f"--watch-pid pid must be int (got {raw!r})") from e
    if pid <= 0:
      raise argparse.ArgumentTypeError(f"--watch-pid pid must be positive (got {raw!r})")
    return {"host": host, "pid": pid}
  try:
    pid = int(raw)
  except ValueError as e:
    raise argparse.ArgumentTypeError(f"--watch-pid must be int or host:int (got {raw!r})") from e
  if pid <= 0:
    raise argparse.ArgumentTypeError(f"--watch-pid pid must be positive (got {raw!r})")
  return {"host": None, "pid": pid}


def main() -> None:
  parser = argparse.ArgumentParser(description="Schedule a delayed trigger for a CharlieBot session")
  parser.add_argument(
      "--session",
      required=False,
      default=None,
      help="Session ID (optional; auto-derived from cwd when run inside a CharlieBot session dir)")
  parser.add_argument(
      "--max-wait",
      required=True,
      type=int,
      help="Max wait in seconds before the trigger fires. When --watch-pid is set, "
      "this is the upper bound; the trigger fires earlier when all watched PIDs exit.",
  )
  parser.add_argument("--message", required=True, help="Message to send to the master CC when the trigger fires")
  parser.add_argument(
      "--watch-pid",
      nargs="+",
      type=_parse_watch_target,
      metavar="PID|HOST:PID",
      default=None,
      help=(
          "Optional PIDs to watch. Each value is either a local PID (e.g. 12345) "
          "or a remote PID of the form HOST:PID (e.g. neptune:12345). All values "
          "in a single trigger must be either all local or all remote — mixing is "
          "rejected. The trigger fires when ALL listed PIDs have exited (ALL-die "
          "semantics) OR when --max-wait elapses, whichever comes first."
      ),
  )
  args = parser.parse_args()
  session_id = resolve_session_id(args.session)

  watch_targets: list[dict] | None = args.watch_pid
  if watch_targets is not None:
    has_local = any(t["host"] is None for t in watch_targets)
    has_remote = any(t["host"] is not None for t in watch_targets)
    if has_local and has_remote:
      parser.error("--watch-pid values must be all local or all remote, not mixed")

  payload: dict = {
      "session_id": session_id,
      "delay_seconds": args.max_wait,
      "message": args.message,
  }
  if watch_targets is not None:
    payload["watch_targets"] = watch_targets

  cfg = get_config()
  try:
    resp = requests.post(
        f"{cfg.server_base_url}/api/internal/schedule-trigger",
        json=payload,
        timeout=HTTP_INTERNAL_API_TIMEOUT,
        verify=False,
    )
  except requests.RequestException as e:
    print(json.dumps({"error": str(e)}), file=sys.stderr)
    sys.exit(1)

  if resp.status_code == 422:
    detail = _extract_detail(resp)
    print(json.dumps({"error": detail}), file=sys.stderr)
    sys.exit(EXIT_VERIFY_REJECTED)
  if not resp.ok:
    detail = _extract_detail(resp)
    print(json.dumps({"error": detail}), file=sys.stderr)
    sys.exit(1)

  print(json.dumps(resp.json(), indent=2))


def _extract_detail(resp: requests.Response) -> str:
  try:
    return resp.json()["detail"]
  except (ValueError, KeyError):
    return resp.text or f"HTTP {resp.status_code}"


if __name__ == "__main__":
  main()
