"""CLI script for master CC to schedule a delayed trigger.

Called by the master Claude Code instance via its run_command tool. ``--session``
is optional; the server-written CHARLIEBOT_SESSION_ID supplies it in normal master
use (see ``resolve_session_id``).

  charliebot schedule-trigger \
    --max-wait 3600 \
    --message 'Check PID 12345'

Optional: watch one or more targets. The trigger fires when ALL targets have
finished OR when --max-wait elapses (whichever comes first). Each --watch spec
self-describes its kind: a bare integer is a local PID (e.g. ``12345``),
``host:pid`` is a remote PID (e.g. ``neptune:12345``), and ``slurm:<jobid>`` is a
SLURM job (e.g. ``slurm:98765``). ``host:slurm:<jobid>`` is a SLURM job on a
remote cluster login host (e.g. ``neptune:slurm:98765``). Kinds may be mixed
freely in one trigger.

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

from src.cli.common import add_session_arg, post_internal_api, resolve_session_id
from src.core.config import get_config
from src.core.models import MAX_TRIGGER_MESSAGE_CHARS, WatchKind

# Exit code returned when trigger creation is rejected: remote-PID verify-on-create
# or --message length validation. Mirrors argparse's usage-error exit code.
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
  (slurm job on this host), ``host:slurm:12345`` (slurm job on a remote cluster
  login host). ``slurm`` is a reserved scheme keyword, so a host literally named
  ``slurm`` cannot be used as a remote target.
  """
  if ":" in raw:
    scheme, _, rest = raw.partition(":")
    if scheme == "slurm":
      return {"kind": WatchKind.SLURM_JOB.value, "job_id": _positive_int(rest, raw, "slurm job id")}
    if not scheme:
      raise argparse.ArgumentTypeError(f"--watch host must be non-empty (got {raw!r})")
    sub_scheme, sep, sub_rest = rest.partition(":")
    if sep and sub_scheme == "slurm":
      return {
          "kind": WatchKind.SLURM_JOB.value,
          "host": scheme,
          "job_id": _positive_int(sub_rest, raw, "slurm job id"),
      }
    return {"kind": WatchKind.REMOTE_PID.value, "host": scheme, "pid": _positive_int(rest, raw, "pid")}
  return {"kind": WatchKind.LOCAL_PID.value, "pid": _positive_int(raw, raw, "pid")}


def _validate_message(value: str) -> str:
  """Reject a --message longer than the short-label limit at argparse time.

  The wake lands back in the same session with full history, so the message is
  only a short label naming which watch fired. This runs before any network
  call, so an over-limit message never reaches the internal API.
  """
  if len(value) > MAX_TRIGGER_MESSAGE_CHARS:
    raise argparse.ArgumentTypeError(
        f"--message must be a short label at most {MAX_TRIGGER_MESSAGE_CHARS} characters "
        f"(got {len(value)})")
  return value


def _build_parser() -> argparse.ArgumentParser:
  parser = argparse.ArgumentParser(description="Schedule a delayed trigger for a CharlieBot session")
  add_session_arg(parser)
  parser.add_argument(
      "--max-wait",
      required=True,
      type=int,
      help="Max wait in seconds before the trigger fires. When --watch is set, "
      "this is the upper bound; the trigger fires earlier when all watched targets finish.",
  )
  parser.add_argument(
      "--message",
      required=True,
      type=_validate_message,
      help=(
          "Short label naming which watch fired, at most "
          f"{MAX_TRIGGER_MESSAGE_CHARS} characters. The wake keeps the session's "
          "full history; runbook steps and readback commands live in session "
          "artifacts."),
  )
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
          "HOST:slurm:JOBID is a SLURM job on a remote cluster login host "
          "(e.g. neptune:slurm:98765). "
          "Kinds may be mixed freely. The trigger fires when ALL targets have "
          "finished (ALL semantics) OR when --max-wait elapses, whichever comes "
          "first."),
  )
  return parser


def _target_matches(stored: dict, requested: dict) -> bool:
  """A stored watch target equals a requested one (server stores model defaults)."""
  if stored.get("kind") != requested.get("kind"):
    return False
  for key in ("pid", "job_id"):
    if requested.get(key) is not None and stored.get(key) != requested.get(key):
      return False
  requested_host = requested.get("host")
  if requested_host is not None and stored.get("host") != requested_host:
    return False
  return requested_host is not None or stored.get("host") is None


def _readback_trigger(session_id: str, message: str, watch_targets: list[dict] | None) -> dict | None:
  """Sent-but-lost: a persisted, still-pending trigger with the same message +
  watch targets.

  The server generates the trigger id, so it cannot be predicted — matching the
  call's own fields is the readback judgment. Fired triggers are never deleted
  (they stay on disk with status "fired"), and self-renewing watches reuse the
  identical message on every renewal, so a fired trigger from a previous leg
  can match; only "pending" proves the call landed. Among several pending
  matches, the newest by ``created_at`` wins.
  """
  triggers_dir = get_config().sessions_dir / session_id / "triggers"
  if not triggers_dir.is_dir():
    return None
  requested = list(watch_targets or [])
  candidates: list[dict] = []
  for trigger_file in triggers_dir.glob("*.json"):
    try:
      stored = json.loads(trigger_file.read_text(encoding="utf-8"))
    except (OSError, ValueError):
      continue
    if stored.get("status") != "pending":
      continue
    if stored.get("message") != message:
      continue
    stored_targets = stored.get("watch_targets") or []
    if len(stored_targets) != len(requested):
      continue
    if not all(any(_target_matches(st, rq) for st in stored_targets) for rq in requested):
      continue
    candidates.append(stored)
  if not candidates:
    return None
  newest = max(candidates, key=lambda t: t["created_at"])
  # Mirror the endpoint's response shape.
  return {"trigger_id": newest["id"], "fire_at": newest["fire_at"]}


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

  result = post_internal_api(
      "/api/internal/schedule-trigger",
      payload,
      readback=lambda: _readback_trigger(session_id, args.message, watch_targets),
      rejection_exit_codes={422: EXIT_VERIFY_REJECTED},
  )
  print(json.dumps(result, indent=2))


if __name__ == "__main__":
  main()
