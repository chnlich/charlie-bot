"""CLI script for master CC to schedule a delayed trigger.

Called by the master Claude Code instance as a shell command (session
identity resolves per ``resolve_session_id``).

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

Alternative: start a command and watch the started process in one step. ``--cmd
CMD`` starts CMD detached through the shared launcher (``--host HOST`` runs it
on that ssh host, ``--cwd DIR`` picks its working directory — locally the
caller's current directory, remotely the ssh login directory), writes the
launch record under the session's ``launches/`` directory, and registers the
wake on the wrapper process; the trigger fires when the command exits or at
--max-wait, whichever comes first, and the wake carries the exit code and the
log path. ``--cmd`` and ``--watch`` are mutually exclusive.

  charliebot schedule-trigger \
    --cmd 'make all' \
    --max-wait 1800 \
    --message 'Build finished'

Exit codes: 0 = registered (stdout prints one JSON object: trigger_id, fire_at
and the launch record fields), 2 = usage error or a rejected/failed
registration (a started process is killed first), 3 = the ssh start failed or
the remote PID did not parse (nothing registered).
"""

from __future__ import annotations

import argparse
import datetime
import json
import os
import sys
from typing import TYPE_CHECKING

from src.infra import constants, help_formatter
from src.runtime.cli import common as cli_common

if TYPE_CHECKING:
  from src.runtime.cli import launcher

# Exit code returned when trigger creation is rejected: remote-PID verify-on-create
# or --message length validation. Mirrors argparse's usage-error exit code.
EXIT_VERIFY_REJECTED = 2

# Exit code returned when the --cmd start over ssh failed or the remote PID did
# not parse: no process was started (or none whose PID is known) and no trigger
# was registered.
EXIT_SSH_FAILED = 3


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
      return {"kind": constants.WatchKind.SLURM_JOB.value, "job_id": _positive_int(rest, raw, "slurm job id")}
    if not scheme:
      raise argparse.ArgumentTypeError(f"--watch host must be non-empty (got {raw!r})")
    sub_scheme, sep, sub_rest = rest.partition(":")
    if sep and sub_scheme == "slurm":
      return {
          "kind": constants.WatchKind.SLURM_JOB.value,
          "host": scheme,
          "job_id": _positive_int(sub_rest, raw, "slurm job id"),
      }
    return {"kind": constants.WatchKind.REMOTE_PID.value, "host": scheme, "pid": _positive_int(rest, raw, "pid")}
  return {"kind": constants.WatchKind.LOCAL_PID.value, "pid": _positive_int(raw, raw, "pid")}


def _validate_message(value: str) -> str:
  """Reject a --message longer than the short-label limit at argparse time.

  The wake lands back in the same session with full history, so the message is
  only a short label naming which watch fired. This runs before any network
  call, so an over-limit message never reaches the internal API.
  """
  if len(value) > constants.MAX_TRIGGER_MESSAGE_CHARS:
    raise argparse.ArgumentTypeError(
        f"--message must be a short label at most {constants.MAX_TRIGGER_MESSAGE_CHARS} characters "
        f"(got {len(value)})")
  return value


def _build_parser() -> argparse.ArgumentParser:
  parser = help_formatter.CliArgumentParser(description="Schedule a delayed trigger for a CharlieBot session")
  cli_common.add_session_arg(parser)
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
          f"{constants.MAX_TRIGGER_MESSAGE_CHARS} characters. The wake keeps the session's "
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
          "first. Mutually exclusive with --cmd."),
  )
  parser.add_argument(
      "--cmd",
      default=None,
      metavar="CMD",
      help=(
          "Start CMD detached and watch the started process: the trigger fires "
          "when it exits or at --max-wait, whichever comes first, and the wake "
          "carries the exit code and the log path. Mutually exclusive with "
          "--watch."),
  )
  parser.add_argument(
      "--host",
      default=None,
      metavar="HOST",
      help="Run --cmd on this ssh host. Requires --cmd.",
  )
  parser.add_argument(
      "--cwd",
      default=None,
      metavar="DIR",
      help=(
          "Working directory for --cmd. Requires --cmd. Defaults to the "
          "current directory locally, and to the ssh login directory with --host."),
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
  triggers_dir = cli_common.get_config().sessions_dir / session_id / "triggers"
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


def _register_and_record(
    args: argparse.Namespace,
    session_id: str,
    launch: launcher.Launch,
    launch_id: str,
    started_at: datetime.datetime,
    cwd: str,
) -> None:
  """Register the started process's wake and write its launch record.

  A rejected or unverifiable registration kills the started process group and
  exits 2: nothing started outlives its wake. On success the launch record
  lands at ``<session>/launches/<launch_id>/metadata.json`` and stdout prints
  trigger_id, fire_at and the record's fields as one JSON object.
  """
  from src.runtime.cli import launcher  # deferred: charliebot schedule-trigger --help

  if launch.host is None:
    target: dict = {
        "kind": constants.WatchKind.LOCAL_PID.value,
        "pid": launch.pid,
        "exit_file": launch.exit_file,
        "log": launch.log,
    }
  else:
    target = {
        "kind": constants.WatchKind.REMOTE_PID.value,
        "host": launch.host,
        "pid": launch.pid,
        "exit_file": launch.exit_file,
        "log": launch.log,
    }
  watch_targets = [target]
  payload: dict = {
      "session_id": session_id,
      "delay_seconds": args.max_wait,
      "message": args.message,
      "watch_targets": watch_targets,
  }
  try:
    result = cli_common.post_internal_api(
        "/api/internal/schedule-trigger",
        payload,
        readback=lambda: _readback_trigger(session_id, args.message, watch_targets),
        rejection_exit_codes={422: EXIT_VERIFY_REJECTED},
    )
  except SystemExit:
    # post_internal_api printed the reason as JSON on stderr; the started
    # process must not outlive its wake.
    launcher.kill_launch(launch)
    sys.exit(EXIT_VERIFY_REJECTED)
  if _readback_trigger(session_id, args.message, watch_targets) is None:
    launcher.kill_launch(launch)
    cli_common.print_json(
        {"error": "registration rejected: no pending trigger watching the started pid on readback"},
        indent=None,
        file=sys.stderr)
    sys.exit(EXIT_VERIFY_REJECTED)

  record = {
      "launch_id": launch_id,
      "session_id": session_id,
      "host": launch.host,
      "pid": launch.pid,
      "cwd": cwd,
      "cmd": args.cmd,
      "started_at": started_at.isoformat().replace("+00:00", "Z"),
      "log": launch.log,
      "exit_file": launch.exit_file,
      "trigger_id": result["trigger_id"],
  }
  launch_dir = cli_common.get_config().sessions_dir / session_id / "launches" / launch_id
  launch_dir.mkdir(parents=True, exist_ok=True)
  (launch_dir / "metadata.json").write_text(json.dumps(record, separators=(",", ":")) + "\n", encoding="utf-8")
  output = {"trigger_id": result["trigger_id"], "fire_at": result["fire_at"], **record}
  cli_common.print_json(output, indent=2, file=sys.stdout)


def _main_cmd(args: argparse.Namespace, session_id: str) -> None:
  """Start --cmd detached through the shared launcher and register its wake."""
  from src.infra import models  # deferred: charliebot schedule-trigger --help
  from src.runtime.cli import launcher  # deferred: charliebot schedule-trigger --help

  started_at = models.utc_now()
  launch_id = launcher.mint_launch_id(started_at)
  if args.host is None:
    cwd = args.cwd or os.getcwd()
    sessions_dir = cli_common.get_config().sessions_dir
    launch_dir = sessions_dir / session_id / "launches" / launch_id
    launch = launcher.launch_local(args.cmd, cwd=cwd, log_path=launch_dir / "log", exit_file=launch_dir / "exit")
  else:
    # The record's "~" names the ssh login directory: without --cwd the remote
    # wrapper runs there and no local path describes it.
    cwd = args.cwd or "~"
    try:
      launch = launcher.launch_remote(args.host, args.cmd, cwd=args.cwd, launch_id=launch_id)
    except launcher.RemoteLaunchError as e:
      cli_common.print_json({"error": str(e)}, indent=None, file=sys.stderr)
      sys.exit(EXIT_SSH_FAILED)
  _register_and_record(args, session_id, launch, launch_id, started_at, cwd)


def main() -> None:
  parser = _build_parser()
  args = parser.parse_args()
  if args.cmd is not None and args.watch is not None:
    cli_common.exit_usage_error(
        "--cmd and --watch are mutually exclusive: the trigger watches the one process --cmd starts")
  if args.host is not None and args.cmd is None:
    cli_common.exit_usage_error("--host requires --cmd: it names the host the started command runs on")
  if args.cwd is not None and args.cmd is None:
    cli_common.exit_usage_error("--cwd requires --cmd: it names the started command's working directory")
  session_id = cli_common.resolve_session_id(args.session)
  if args.cmd is not None:
    _main_cmd(args, session_id)
    return

  watch_targets: list[dict] | None = args.watch

  payload: dict = {
      "session_id": session_id,
      "delay_seconds": args.max_wait,
      "message": args.message,
  }
  if watch_targets is not None:
    payload["watch_targets"] = watch_targets

  result = cli_common.post_internal_api(
      "/api/internal/schedule-trigger",
      payload,
      readback=lambda: _readback_trigger(session_id, args.message, watch_targets),
      rejection_exit_codes={422: EXIT_VERIFY_REJECTED},
  )
  cli_common.print_json(result, indent=2, file=sys.stdout)


if __name__ == "__main__":
  main()
