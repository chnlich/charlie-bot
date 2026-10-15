"""Shared detached-process launcher: one wrapper per start, on this host or over ssh.

Every start runs the wrapper ``(CMD); echo $? > EXIT_FILE`` under ``bash -lc``,
with stdin from /dev/null and stdout+stderr going to the launch's log file. The
watched PID is the wrapper itself — taken from the local subprocess object, or
from the remote shell's ``$!`` over ssh — and the wrapper exits only after the
exit file holds the command's exit code, so a wake that sees the PID gone can
read the exit code out of the exit file. The wrapper leads its own process
group on both paths (``start_new_session`` locally, ``setsid`` remotely), which
makes its PID the group id the rejection path kills. ``schedule-trigger --cmd``
starts through here; the fire-time exit reads live in ``src/runtime/triggers.py``.
"""

from __future__ import annotations

import contextlib
import dataclasses
import datetime
import os
import pathlib
import shlex
import signal
import subprocess
import sys

from src.infra import ssh, timeouts

# The remote side of one launch stages its files under this directory, named by launch id.
REMOTE_RUNS_DIR = "/tmp/charliebot_runs"


@dataclasses.dataclass(frozen=True)
class Launch:
  """One started wrapper: the watched PID, its host (None = local), and the two paths its wake reads."""
  pid: int
  host: str | None
  log: str
  exit_file: str


class RemoteLaunchError(Exception):
  """The ssh start failed or the remote PID did not parse; no launch record and no trigger exist."""


def mint_launch_id(started_at: datetime.datetime) -> str:
  """The launch id format ``<UTC %Y%m%dT%H%M%S>-<6 hex>``; secrets rides the one start that mints an id."""
  import secrets

  return f"{started_at:%Y%m%dT%H%M%S}-{secrets.token_hex(3)}"


def _wrapper_command(cmd: str, exit_file: str) -> str:
  """The bash -lc payload: run CMD in a subshell, then publish its exit code.

  The echo stays outside the subshell: an ``exit`` builtin inside CMD ends the
  subshell only, and the wrapper still writes the exit file before it exits.
  The newline before ``)`` keeps a trailing ``# comment`` in CMD from swallowing it.
  """
  return f"({cmd}\n); echo $? > {shlex.quote(exit_file)}"


def launch_local(cmd: str, *, cwd: str, log_path: pathlib.Path, exit_file: pathlib.Path) -> Launch:
  """Start the wrapper on this host as a new-session process; the log is appended to."""
  log_path.parent.mkdir(parents=True, exist_ok=True)
  exit_file.parent.mkdir(parents=True, exist_ok=True)
  with log_path.open("a", encoding="utf-8") as log:
    proc = subprocess.Popen(
        ["bash", "-lc", _wrapper_command(cmd, str(exit_file))],
        stdin=subprocess.DEVNULL,
        stdout=log,
        stderr=subprocess.STDOUT,
        cwd=cwd,
        start_new_session=True)
  return Launch(pid=proc.pid, host=None, log=str(log_path), exit_file=str(exit_file))


def launch_remote(host: str, cmd: str, *, cwd: str | None, launch_id: str) -> Launch:
  """Start the wrapper on *host* over ssh+setsid and return its remote PID.

  Log, exit file and PID file live under ``/tmp/charliebot_runs/<launch_id>/``;
  without *cwd* the command runs in the ssh login directory. Raises
  RemoteLaunchError on an ssh failure or an unparseable PID.
  """
  remote_dir = f"{REMOTE_RUNS_DIR}/{launch_id}"
  remote_log = f"{remote_dir}/log"
  remote_exit = f"{remote_dir}/exit"
  remote_pid_file = f"{remote_dir}/pid"

  # The brace group carries the stream redirections for both wrapper steps, so
  # the backgrounded setsid process holds no ssh channel fd and ssh returns.
  inner = f"{{ {_wrapper_command(cmd, remote_exit)}; }} < /dev/null > {shlex.quote(remote_log)} 2>&1"
  cd = f"cd {shlex.quote(cwd)} && " if cwd is not None else ""
  wrapper = (
      f"mkdir -p {shlex.quote(remote_dir)} && "
      f"{cd}"
      f"{{ setsid bash -lc {shlex.quote(inner)} & "
      f"echo $! > {shlex.quote(remote_pid_file)} && "
      f"cat {shlex.quote(remote_pid_file)}; }}")

  try:
    proc = subprocess.run(
        ssh.ssh_cmd(host, "bash", "-c", shlex.quote(wrapper)),
        capture_output=True,
        text=True,
        check=False,
        timeout=timeouts.SSH_LAUNCH_TIMEOUT,
    )
  except subprocess.TimeoutExpired as exc:
    stderr = exc.stderr or ""
    raise RemoteLaunchError(f"ssh to {host} timed out after {timeouts.SSH_LAUNCH_TIMEOUT}s: {stderr.strip()}") from exc
  if proc.returncode != 0:
    raise RemoteLaunchError(f"ssh to {host} failed (rc={proc.returncode}): {proc.stderr.strip()}")

  lines = [ln for ln in proc.stdout.splitlines() if ln.strip()]
  try:
    pid = int(lines[-1].strip())
  except (IndexError, ValueError) as exc:
    raise RemoteLaunchError(f"failed to parse remote PID from ssh stdout: {proc.stdout!r}") from exc
  return Launch(pid=pid, host=host, log=remote_log, exit_file=remote_exit)


def kill_launch(launch: Launch) -> None:
  """Best-effort kill of the wrapper's process group after a rejected registration.

  The wrapper leads its group on both paths, so one kill by group id takes the
  whole tree; an already-exited wrapper meets the goal. A remote kill that
  could not run is warned about, not hidden: the registration failure is
  already reported, and the caller's exit code does not change.
  """
  if launch.host is None:
    with contextlib.suppress(ProcessLookupError):
      os.killpg(launch.pid, signal.SIGKILL)
    # The wrapper is this process's child: reap it so the rejection is reported
    # only after the started PID actually stops answering kill -0 (a zombie
    # still answers). Not-our-child doubles and already-reaped wrappers meet
    # the goal as they are.
    with contextlib.suppress(ChildProcessError):
      os.waitpid(launch.pid, 0)
    return
  try:
    proc = subprocess.run(
        ssh.ssh_cmd(launch.host, f"kill -KILL -- -{launch.pid}"),
        capture_output=True,
        text=True,
        check=False,
        timeout=timeouts.SSH_LAUNCH_TIMEOUT,
    )
  except subprocess.TimeoutExpired:
    print(
        f"warning: remote kill of {launch.host}:{launch.pid} timed out; the process group may still run",
        file=sys.stderr)
    return
  if proc.returncode != 0 and "no such process" not in proc.stderr.lower():
    print(
        f"warning: remote kill of {launch.host}:{launch.pid} exited {proc.returncode}: {proc.stderr.strip()}",
        file=sys.stderr)
