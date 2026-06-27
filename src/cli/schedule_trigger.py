"""CLI script for master CC to schedule a delayed trigger.

Called by the master Claude Code instance via its run_command tool. ``--session``
is optional; auto-derived from cwd in normal master use.

  charliebot schedule-trigger \
    --max-wait 3600 \
    --message 'Check PID 12345'

Optional: watch one or more targets. The trigger fires when ALL targets have
finished OR when --max-wait elapses (whichever comes first). Each --watch spec
self-describes its kind: a bare integer is a local PID (e.g. ``12345``),
``host:pid`` is a remote PID (e.g. ``neptune:12345``), and ``slurm:<jobid>`` is a
SLURM job (e.g. ``slurm:98765``). Kinds may be mixed freely in one trigger.

  charliebot schedule-trigger \
    --max-wait 3600 \
    --watch 12345 67890 \
    --message 'Training finished'

  charliebot schedule-trigger \
    --max-wait 3600 \
    --watch neptune:12345 slurm:98765 \
    --message 'Remote + slurm training finished'
"""

import argparse
import json
import sys

import requests

from src.cli.common import internal_api_auth_headers, resolve_session_id
from src.core.config import get_config
from src.core.models import WatchKind
from src.core.timeouts import HTTP_INTERNAL_API_TIMEOUT

# Exit code returned when remote-PID verify-on-create rejects the trigger.
EXIT_VERIFY_REJECTED = 2


def _positive_int(value: str, raw: str, what: str) -> int:
  """Parse ``value`` as a positive int, raising a parser-friendly error on failure."""
  try:
    n = int(value)
  except ValueError as e:
    raise argparse.ArgumentTypeError(f"--watch {what} must be int (got {raw!r})") from e
  if n <= 0:
    raise argparse.ArgumentTypeError(f"--watch {what} must be positive (got {raw!r})")
  return n


def _parse_watch_target(raw: str) -> dict:
  """Parse a single --watch spec into a watch-target dict carrying its kind.

  Forms: ``12345`` (local pid), ``host:12345`` (remote pid), ``slurm:12345``
  (slurm job). ``slurm`` is a reserved scheme keyword.
  """
  if ":" in raw:
    scheme, _, rest = raw.partition(":")
    if scheme == "slurm":
      return {"kind": WatchKind.SLURM_JOB.value, "job_id": _positive_int(rest, raw, "slurm job id")}
    if not scheme:
      raise argparse.ArgumentTypeError(f"--watch host must be non-empty (got {raw!r})")
    return {"kind": WatchKind.REMOTE_PID.value, "host": scheme, "pid": _positive_int(rest, raw, "pid")}
  return {"kind": WatchKind.LOCAL_PID.value, "pid": _positive_int(raw, raw, "pid")}


def _build_parser() -> argparse.ArgumentParser:
  parser = argparse.ArgumentParser(description="Schedule a delayed trigger for a CharlieBot session")
  parser.add_argument(
      "--session",
      required=False,
      default=None,
      help="Session ID (optional; auto-derived from cwd)")
  parser.add_argument(
      "--max-wait",
      required=True,
      type=int,
      help="Max wait in seconds before the trigger fires. When --watch is set, "
      "this is the upper bound; the trigger fires earlier when all watched targets finish.",
  )
  parser.add_argument("--message", required=True, help="Message to send to the master CC when the trigger fires")
  parser.add_argument(
      "--watch",
      nargs="+",
      type=_parse_watch_target,
      metavar="SPEC",
      default=None,
      help=(
          "Optional targets to watch. Each SPEC self-describes its kind: a bare "
          "integer is a local PID (e.g. 12345), HOST:PID is a remote PID (e.g. "
          "neptune:12345), and slurm:JOBID is a SLURM job (e.g. slurm:98765). "
          "Kinds may be mixed freely. The trigger fires when ALL targets have "
          "finished (ALL semantics) OR when --max-wait elapses, whichever comes "
          "first."),
  )
  return parser


def main() -> None:
  parser = _build_parser()
  args = parser.parse_args()
  session_id = resolve_session_id(args.session)

  watch_targets: list[dict] | None = args.watch

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
        headers=internal_api_auth_headers(cfg),
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
