"""CLI script for launching a long-running remote command via ssh+setsid+nohup.

Captures the remote PID, stages stdout/stderr/sentinel under a remote dir, and
writes a local metadata.json describing the launch.

  python -m src.cli.remote_launch \
    --session SESSION_ID \
    --host HOST \
    --cwd CWD \
    --cmd CMD

Exit codes:
  0 - success
  2 - ssh failed (network/auth/timeout/non-zero return)
  3 - remote PID parse failed
  4 - local session dir missing
"""

import argparse
import json
import secrets
import shlex
import subprocess
import sys
from pathlib import Path

from src.core.models import utc_now


def _ssh_launch_remote(host: str, cwd: str, cmd: str, launch_id: str) -> int:
  remote_dir = f"/tmp/charliebot_runs/{launch_id}"
  remote_log = f"{remote_dir}/log"
  remote_sentinel = f"{remote_dir}/sentinel"
  remote_pid_file = f"{remote_dir}/pid"

  inner = f"({cmd}; echo $? > {remote_sentinel}) > {remote_log} 2>&1"
  wrapper = (
      f"mkdir -p {remote_dir} && "
      f"cd {shlex.quote(cwd)} && "
      f"{{ setsid bash -lc {shlex.quote(inner)} & "
      f"echo $! > {remote_pid_file} && "
      f"cat {remote_pid_file}; }}")

  try:
    proc = subprocess.run(
        ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", host, "bash", "-c",
         shlex.quote(wrapper)],
        capture_output=True,
        text=True,
        check=False,
        timeout=30,
    )
  except subprocess.TimeoutExpired as exc:
    stderr = exc.stderr or ""
    print(f"ssh to {host} timed out after 30s: {stderr.strip()}", file=sys.stderr)
    sys.exit(2)
  if proc.returncode != 0:
    print(f"ssh to {host} failed (rc={proc.returncode}): {proc.stderr.strip()}", file=sys.stderr)
    sys.exit(2)

  lines = [ln for ln in proc.stdout.splitlines() if ln.strip()]
  try:
    return int(lines[-1].strip())
  except (IndexError, ValueError):
    print(f"failed to parse remote PID from ssh stdout: {proc.stdout!r}", file=sys.stderr)
    sys.exit(3)


def main() -> None:
  parser = argparse.ArgumentParser(description="Launch a long-running command on a remote host via ssh+setsid")
  parser.add_argument("--session", required=True, help="Session ID")
  parser.add_argument("--host", required=True, help="Remote host (ssh target)")
  parser.add_argument("--cwd", required=True, help="Working directory on the remote host")
  parser.add_argument("--cmd", required=True, help="Command to execute on the remote host")
  args = parser.parse_args()

  started_at = utc_now()
  launch_id = f"{started_at:%Y%m%dT%H%M%S}-{secrets.token_hex(3)}"

  remote_pid = _ssh_launch_remote(args.host, args.cwd, args.cmd, launch_id)

  session_dir = Path.home() / ".charliebot" / "sessions" / args.session
  if not session_dir.is_dir():
    print(f"session dir does not exist: {session_dir}", file=sys.stderr)
    sys.exit(4)

  launch_dir = session_dir / "launches" / launch_id
  launch_dir.mkdir(parents=True, exist_ok=True)

  metadata = {
      "launch_id": launch_id,
      "session_id": args.session,
      "host": args.host,
      "remote_pid": remote_pid,
      "cwd": args.cwd,
      "cmd": args.cmd,
      "started_at": started_at.isoformat().replace("+00:00", "Z"),
  }
  metadata_json = json.dumps(metadata, separators=(",", ":"))
  (launch_dir / "metadata.json").write_text(metadata_json + "\n")
  print(metadata_json)


if __name__ == "__main__":
  main()
