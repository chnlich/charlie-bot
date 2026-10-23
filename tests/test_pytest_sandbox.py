"""The sandbox plugin restricts a pytest run to the processes it started.

Each test that needs an unreachable target starts it in the outer test, then runs
an inner pytest in a subprocess; the inner run is a nested Landlock domain and
cannot signal the outer test's processes (tests/test_pytest_sandbox.py invariants).
"""

import os
import pathlib
import signal
import subprocess
import sys
import uuid

import pytest

# Every test here observes the plugin; without the sandbox they would fail for the wrong reason.
pytestmark = pytest.mark.skipif(
    os.environ.get("CHARLIEBOT_USE_TEST_SANDBOX", "1") != "1",
    reason="the sandbox boundaries need CHARLIEBOT_USE_TEST_SANDBOX=1")

_INNER = """\
import os, signal, subprocess, sys

import pytest

def test_inner():
{body}
"""


def _start_sentinel(marker: str) -> subprocess.Popen:
  """Start a sleep process whose argv[0] carries the marker."""
  return subprocess.Popen(["bash", "-c", f"exec -a {marker} sleep 300"])


def _run_inner(tmp_path: pathlib.Path, body: str) -> subprocess.CompletedProcess:
  test_file = tmp_path / "test_inner.py"
  test_file.write_text(_INNER.format(body=body))
  return subprocess.run(
      [sys.executable, "-m", "pytest", "-q", "-p", "no:cacheprovider",
       str(test_file)],
      capture_output=True,
      text=True,
      cwd=tmp_path,
      timeout=60,
  )


def test_outer_sentinel_is_unreachable(tmp_path: pathlib.Path) -> None:
  """A nested pytest run sees the outer sentinel in pgrep but cannot signal it."""
  marker = f"sbx-unreachable-{uuid.uuid4().hex}"
  sentinel = _start_sentinel(marker)
  try:
    result = _run_inner(
        tmp_path, f"""
    r = subprocess.run(["pgrep", "-f", "{marker}"], capture_output=True, text=True)
    assert r.returncode == 0, r.stdout
    pid = int(r.stdout.split()[0])
    pidfd = os.pidfd_open(pid)
    for send in (lambda: os.kill(pid, signal.SIGTERM),
                 lambda: signal.pidfd_send_signal(pidfd, signal.SIGTERM)):
      with pytest.raises(PermissionError):
        send()
    """)
    assert result.returncode == 0, result.stdout + result.stderr
    os.kill(sentinel.pid, 0)  # the sentinel is still alive after the run
  finally:
    sentinel.kill()
    sentinel.wait()


def test_own_processes_are_signalable() -> None:
  """A run kills the child and grandchild it started with SIGTERM and SIGKILL."""
  child = subprocess.Popen(["bash", "-c", f"exec -a sbx-child-{uuid.uuid4().hex} sleep 300"])
  grandchild = subprocess.Popen(["sleep", "300"])
  try:
    os.kill(child.pid, signal.SIGTERM)
    child.wait(timeout=10)
    assert child.returncode == -signal.SIGTERM
    os.kill(grandchild.pid, signal.SIGKILL)
    grandchild.wait(timeout=10)
    assert grandchild.returncode == -signal.SIGKILL
  finally:
    for proc in (child, grandchild):
      if proc.poll() is None:
        proc.kill()
        proc.wait()


def test_tmux_servers_are_isolated(tmp_path: pathlib.Path) -> None:
  """A run cannot reach a tmux server started outside it and cleans its own socket dir."""
  marker = f"sbx-tmux-{uuid.uuid4().hex}"
  other = f"sbx-other-{uuid.uuid4().hex}"
  sentinel = subprocess.Popen(
      ["tmux", "-L", marker, "-f", "/dev/null", "new-session", "-d", "-s", "s"],
      stdout=subprocess.DEVNULL,
      stderr=subprocess.DEVNULL,
  )
  sentinel.wait(timeout=10)
  try:
    result = _run_inner(
        tmp_path, f"""
    import glob, os, pathlib, subprocess
    for cmd in (["tmux", "-L", "{marker}", "ls"], ["tmux", "ls"]):
      r = subprocess.run(cmd, capture_output=True, text=True)
      assert r.returncode != 0, (cmd, r.stdout)
    r = subprocess.run(["tmux", "-L", "{other}", "-f", "/dev/null", "new-session", "-d"],
                       capture_output=True, text=True)
    assert r.returncode == 0, r.stderr
    sockets = glob.glob(os.environ["TMUX_TMPDIR"] + "/tmux-*/*")
    assert any("{other}" in s for s in sockets), sockets
    subprocess.run(["tmux", "-S", [s for s in sockets if "{other}" in s][0], "kill-server"],
                   check=True, capture_output=True)
    pathlib.Path("inner_tmpdir").write_text(os.environ["TMUX_TMPDIR"])
    """)
    assert result.returncode == 0, result.stdout + result.stderr
    r = subprocess.run(["tmux", "-L", marker, "ls"], capture_output=True, text=True)
    assert r.returncode == 0, r.stderr  # the sentinel server still answers
    inner_tmpdir = (tmp_path / "inner_tmpdir").read_text()
    assert not pathlib.Path(inner_tmpdir).exists()  # the inner run's TMUX_TMPDIR is gone
  finally:
    subprocess.run(["tmux", "-L", marker, "kill-server"], capture_output=True)


def test_tmux_without_tmpdir_uses_overflow_uid(tmp_path: pathlib.Path) -> None:
  """Without TMUX_TMPDIR a namespaced tmux server sockets under /tmp/tmux-65534/."""
  other = f"sbx-overflow-{uuid.uuid4().hex}"
  result = _run_inner(
      tmp_path, f"""
  import os, subprocess
  env = {{k: v for k, v in os.environ.items() if k != "TMUX_TMPDIR"}}
  r = subprocess.run(["tmux", "-L", "{other}", "-f", "/dev/null", "new-session", "-d"],
                     capture_output=True, text=True, env=env)
  assert r.returncode == 0, r.stderr
  socket = "/tmp/tmux-65534/{other}"
  assert os.path.exists(socket), socket
  subprocess.run(["tmux", "-S", socket, "kill-server"], check=True, capture_output=True)
  """)
  assert result.returncode == 0, result.stdout + result.stderr
  # Only files this test created live in the overflow directory; remove the recorded path.
  pathlib.Path("/tmp/tmux-65534/" + other).unlink(missing_ok=True)


def test_systemctl_user_has_no_bus() -> None:
  """systemctl --user cannot reach a bus from inside a restricted run."""
  r = subprocess.run(["systemctl", "--user", "is-system-running"], capture_output=True, text=True)
  assert r.returncode != 0
  assert "Failed to connect to bus" in r.stdout + r.stderr


def test_abi_below_minimum_refuses(tmp_path: pathlib.Path) -> None:
  """A patched ABI reader below 6 makes pytest exit before collection, naming the switch."""
  site = tmp_path / "site"
  site.mkdir()
  (site / "sitecustomize.py").write_text("import src.infra.pytest_sandbox as m\n"
                                         "m.landlock_abi = lambda: 5\n")
  pythonpath = str(site) + os.pathsep + os.environ.get("PYTHONPATH", "")
  env = dict(os.environ, PYTHONPATH=pythonpath)
  result = subprocess.run(
      [sys.executable, "-m", "pytest", "-q", "-p", "no:cacheprovider", "--co",
       str(tmp_path)],
      capture_output=True,
      text=True,
      cwd=tmp_path,
      env=env,
      timeout=60)
  assert result.returncode != 0
  assert "CHARLIEBOT_USE_TEST_SANDBOX" in result.stderr
