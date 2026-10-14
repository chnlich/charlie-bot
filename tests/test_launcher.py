"""Tests for the shared detached-process launcher (src/runtime/cli/launcher.py).

The ssh runs are doubled through subprocess.run (the launcher's one shell-out);
the local starts run real short-lived wrappers.
"""

import os
import pathlib
import shlex
import shutil
import subprocess
import time
from unittest import mock

import conftest
import pytest

from src.runtime.cli import launcher


def _has_ssh_localhost() -> bool:
  """Return True if ssh to localhost works without password (BatchMode)."""
  try:
    proc = subprocess.run(
        ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=5", "localhost", "true"],
        capture_output=True,
        check=False,
        timeout=10,
    )
    return proc.returncode == 0
  except (subprocess.TimeoutExpired, FileNotFoundError):
    return False


def _pid_alive(pid: int) -> bool:
  try:
    os.kill(pid, 0)
  except ProcessLookupError:
    return False
  return True


def _reap(pid: int, timeout: float = 5) -> None:
  """Waitpid the launcher's wrapper: it is this process's child, so a dead one stays a zombie
  (and kill -0-alive) until reaped."""
  deadline = time.monotonic() + timeout
  while time.monotonic() < deadline:
    try:
      reaped, _status = os.waitpid(pid, os.WNOHANG)
    except ChildProcessError:
      return
    if reaped == pid:
      return
    time.sleep(0.05)
  raise TimeoutError(f"pid {pid} not reaped after {timeout}s")


# ---------------------------------------------------------------------------
# launch_local: the wrapper runs the command, writes the exit file, then exits
# ---------------------------------------------------------------------------


def test_launch_local_exit_code_lands_in_the_exit_file(tmp_path: pathlib.Path) -> None:
  launch = launcher.launch_local(
      "exit 3",
      cwd=str(tmp_path),
      log_path=tmp_path / "launch" / "log",
      exit_file=tmp_path / "launch" / "exit",
  )
  try:
    conftest._wait_for(lambda: pathlib.Path(launch.exit_file).exists(), 5, "the exit file never appeared")
    assert pathlib.Path(launch.exit_file).read_text().strip() == "3"
    # The wrapper exits only after the exit file is written.
    _reap(launch.pid)
    assert not _pid_alive(launch.pid)
  finally:
    launcher.kill_launch(launch)


def test_launch_local_runs_in_cwd_and_logs_output(tmp_path: pathlib.Path) -> None:
  launch = launcher.launch_local(
      "pwd; echo hello",
      cwd=str(tmp_path),
      log_path=tmp_path / "log",
      exit_file=tmp_path / "exit",
  )
  try:
    conftest._wait_for(lambda: pathlib.Path(launch.exit_file).exists(), 5, "the exit file never appeared")
    out = pathlib.Path(launch.log).read_text()
    assert str(tmp_path) in out
    assert "hello" in out
    assert pathlib.Path(launch.exit_file).read_text().strip() == "0"
  finally:
    launcher.kill_launch(launch)


def test_launch_local_trailing_comment_in_cmd_still_writes_the_exit_file(tmp_path: pathlib.Path) -> None:
  launch = launcher.launch_local(
      "echo hi # trailing comment",
      cwd=str(tmp_path),
      log_path=tmp_path / "log",
      exit_file=tmp_path / "exit",
  )
  try:
    conftest._wait_for(lambda: pathlib.Path(launch.exit_file).exists(), 5, "the exit file never appeared")
    assert pathlib.Path(launch.exit_file).read_text().strip() == "0"
    assert pathlib.Path(launch.log).read_text().strip() == "hi"
  finally:
    launcher.kill_launch(launch)


def test_kill_launch_local_kills_the_process_group(tmp_path: pathlib.Path) -> None:
  launch = launcher.launch_local(
      "sleep 600",
      cwd=str(tmp_path),
      log_path=tmp_path / "log",
      exit_file=tmp_path / "exit",
  )
  assert _pid_alive(launch.pid)
  launcher.kill_launch(launch)
  _reap(launch.pid)
  assert not _pid_alive(launch.pid)
  # The group kill reaches the sleep too: no exit file is ever written.
  assert not pathlib.Path(launch.exit_file).exists()


# ---------------------------------------------------------------------------
# launch_remote: the ssh + setsid wrapper, its PID capture, its failures
# ---------------------------------------------------------------------------


def test_launch_remote_parses_the_wrapper_pid() -> None:
  fake_proc = subprocess.CompletedProcess(args=[], returncode=0, stdout="24680\n", stderr="")
  with mock.patch("subprocess.run", return_value=fake_proc) as run_mock:
    launch = launcher.launch_remote("remote.example.com", "echo hi", cwd="/data", launch_id="20261008T000000-abc123")

  assert launch == launcher.Launch(
      pid=24680,
      host="remote.example.com",
      log="/tmp/charliebot_runs/20261008T000000-abc123/log",
      exit_file="/tmp/charliebot_runs/20261008T000000-abc123/exit",
  )
  remote_argv = run_mock.call_args.args[0]
  assert remote_argv[0] == "ssh"
  # The wrapper holds the setsid start, the PID capture and the exit-code write.
  wrapper = remote_argv[-1]
  assert "setsid bash -lc" in wrapper
  assert "echo $!" in wrapper
  assert "{ (echo hi\n); echo $? > EXIT; }" in wrapper.replace(
      shlex.quote("/tmp/charliebot_runs/20261008T000000-abc123/exit"), "EXIT")
  # The redirections sit on the brace group: the exit-code write is not one of them.
  assert "} < /dev/null >" in wrapper
  assert "cd /data" in wrapper


def test_launch_remote_without_cwd_runs_in_the_login_directory() -> None:
  fake_proc = subprocess.CompletedProcess(args=[], returncode=0, stdout="24680\n", stderr="")
  with mock.patch("subprocess.run", return_value=fake_proc) as run_mock:
    launcher.launch_remote("remote.example.com", "echo hi", cwd=None, launch_id="20261008T000000-abc123")
  assert "cd " not in run_mock.call_args.args[0][-1]


def test_launch_remote_ssh_failure_raises() -> None:
  with pytest.raises(launcher.RemoteLaunchError, match="ssh"):
    launcher.launch_remote("nonexistent.invalid", "echo hi", cwd="/tmp", launch_id="20261008T000000-abc123")


def test_launch_remote_unparseable_pid_raises() -> None:
  fake_proc = subprocess.CompletedProcess(args=[], returncode=0, stdout="not-a-pid\n", stderr="")
  with mock.patch("subprocess.run", return_value=fake_proc), \
      pytest.raises(launcher.RemoteLaunchError, match="failed to parse remote PID"):
    launcher.launch_remote("remote.example.com", "echo hi", cwd="/tmp", launch_id="20261008T000000-abc123")


@pytest.mark.skipif(not _has_ssh_localhost(), reason="ssh localhost not available without password")
@pytest.mark.local_only
def test_launch_remote_end_to_end_localhost() -> None:
  from src.infra import models  # deferred: the local_only e2e alone needs the timestamp helper

  launch_id = launcher.mint_launch_id(models.utc_now())
  launch = launcher.launch_remote("localhost", "sleep 0.2; echo hi", cwd="/tmp", launch_id=launch_id)
  remote_dir = pathlib.Path(f"/tmp/charliebot_runs/{launch_id}")
  try:
    assert isinstance(launch.pid, int)
    assert launch.host == "localhost"
    conftest._wait_for(lambda: (remote_dir / "exit").exists(), 5, "the remote exit file never appeared")
    assert (remote_dir / "exit").read_text().strip() == "0"
    assert "hi" in (remote_dir / "log").read_text()
  finally:
    launcher.kill_launch(launch)
    shutil.rmtree(remote_dir, ignore_errors=True)
