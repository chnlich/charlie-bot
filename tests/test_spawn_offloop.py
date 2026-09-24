"""Off-loop spawn contracts for src/agents/backends/spawn.py.

The backend launch family (master turns, workers, one-shots) spawns through
``spawn_subprocess``: the fork+exec handshake parks on a worker thread so the
server's multi-GB resident set never prices its page-table copy onto the event
loop, while the preexec composition still runs in the child exactly as
``asyncio.create_subprocess_exec`` ran it. The off-loop property is pinned by
blocking the child inside its preexec and asserting the loop keeps ticking.
"""

from __future__ import annotations

import asyncio
import os
import signal
import sys
import time
from pathlib import Path

import pytest
from conftest import loop_stall_gaps

from src.agents.backends.spawn import SpawnedProcess, spawn_subprocess

LIMIT = 1024 * 1024

_PY_PRINT = (
    "import sys"
    "; sys.stdout.write('line-one\\nline-two\\n'); sys.stdout.flush()"
    "; sys.stderr.write('err-one\\n'); sys.stderr.flush()")

_PY_ECHO_STDIN = (
    "import sys; data = sys.stdin.buffer.read()"
    "; sys.stdout.buffer.write(b'echo:' + data); sys.stdout.buffer.flush()")


async def _spawn(*args: str, **kwargs) -> SpawnedProcess:
  defaults: dict = dict(
      cwd="/tmp",
      env=dict(os.environ),
      stdin=asyncio.subprocess.DEVNULL,
      stdout=asyncio.subprocess.PIPE,
      stderr=asyncio.subprocess.PIPE,
      limit=LIMIT,
      start_new_session=True,
      preexec_fn=None)
  defaults.update(kwargs)
  return await spawn_subprocess(*args, **defaults)


@pytest.mark.asyncio
async def test_piped_streams_lines_and_exit_code() -> None:
  proc = await _spawn(sys.executable, "-c", _PY_PRINT)
  assert proc.pid > 0
  assert await proc.stdout.readline() == b"line-one\n"
  assert await proc.stdout.readline() == b"line-two\n"
  assert await proc.stderr.readline() == b"err-one\n"
  code = await proc.wait()
  assert proc.returncode == 0 and code == 0
  assert await proc.wait() == 0  # a second waiter reads the same resolved exit
  assert await proc.stdout.read() == b""


@pytest.mark.asyncio
async def test_exited_before_read_still_delivers_buffered_output() -> None:
  proc = await _spawn(sys.executable, "-c", _PY_PRINT)
  await proc.wait()
  # The child exited before any read: the pipe buffer's bytes still deliver,
  # then EOF, the shape _read_server_url's startup-line scan rides.
  assert await proc.stdout.readline() == b"line-one\n"
  rest = await proc.stdout.read()
  assert rest == b"line-two\n"
  assert await proc.stderr.read() == b"err-one\n"


@pytest.mark.asyncio
async def test_killed_child_reports_signal_exit() -> None:
  proc = await _spawn(sys.executable, "-c", "import time; time.sleep(30)")
  await asyncio.sleep(0.05)
  os.kill(proc.pid, signal.SIGKILL)
  assert await asyncio.wait_for(proc.wait(), timeout=5) == -signal.SIGKILL


@pytest.mark.asyncio
async def test_devnull_stdin_and_raw_fd_redirect() -> None:
  out_path = Path("/tmp") / f"spawn-offloop-{os.getpid()}.log"
  fd = os.open(out_path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
  try:
    proc = await _spawn(sys.executable, "-c", "print('to-file')", stdout=fd)
    # stdout redirected to a file descriptor stays unwired, the asyncio
    # transport's own shape for non-PIPE redirects.
    assert proc.stdout is None and proc.stdin is None and proc.stderr is not None
    await proc.wait()
    assert out_path.read_text() == "to-file\n"
  finally:
    os.close(fd)
    out_path.unlink(missing_ok=True)


@pytest.mark.asyncio
async def test_stdin_pipe_write_drain_close() -> None:
  proc = await _spawn(sys.executable, "-c", _PY_ECHO_STDIN, stdin=asyncio.subprocess.PIPE)
  assert proc.stdin is not None
  proc.stdin.write(b"payload")
  await proc.stdin.drain()
  proc.stdin.close()
  await proc.stdin.wait_closed()
  assert await asyncio.wait_for(proc.stdout.read(), timeout=5) == b"echo:payload"


@pytest.mark.asyncio
async def test_fork_parks_off_the_event_loop(tmp_path: Path) -> None:
  """A preexec that sleeps in the child must not stall the spawning loop.

  The old on-loop spawn blocked the caller until the child's exec handshake
  completed, so a sleeping preexec priced its whole sleep onto the loop; the
  off-loop seam leaves the loop ticking while the worker thread waits.
  """
  preexec_ran = tmp_path / "preexec-ran"
  block = tmp_path / "preexec-release"

  def sleeping_preexec() -> None:
    preexec_ran.write_text("1")
    deadline = time.monotonic() + 10.0
    while not block.exists() and time.monotonic() < deadline:
      time.sleep(0.01)

  async with loop_stall_gaps() as gaps:
    spawn_task = asyncio.ensure_future(
        _spawn(sys.executable, "-c", "import time; time.sleep(1)", preexec_fn=sleeping_preexec))
    deadline = time.monotonic() + 10
    while not preexec_ran.exists() and time.monotonic() < deadline:
      await asyncio.sleep(0.01)
    assert preexec_ran.exists()  # the preexec ran, in the child
    await asyncio.sleep(0.2)  # the loop ticks while the child's preexec sleeps
    block.write_text("1")  # release the preexec; the spawn proceeds to exec and wiring
    proc = await asyncio.wait_for(spawn_task, timeout=10)
  os.kill(proc.pid, signal.SIGKILL)
  await asyncio.wait_for(proc.wait(), timeout=5)
  assert max(gaps) < 0.15  # the loop never waited for the child's preexec sleep


# The vfork spawn seam (pdeathsig=True): the piped transports' clone(CLONE_VM|
# CLONE_VFORK) path. The compiled stub is Linux-only and needs an install with
# a C toolchain, so these tests fail loud when the module is missing.
@pytest.mark.skipif(sys.platform != "linux", reason="the vfork stub is Linux-only")
@pytest.mark.asyncio
async def test_vfork_piped_streams_cwd_env_and_exit_code() -> None:
  proc = await _spawn(sys.executable, "-c",
                      "import sys, os; sys.stdout.write(os.getcwd() + '\\n')"
                      "; sys.stderr.write(os.environ['MARKER'] + '\\n')",
                      cwd="/usr", env={**os.environ, "MARKER": "vfk"}, pdeathsig=True)
  assert await proc.stdout.readline() == b"/usr\n"
  assert await proc.stderr.readline() == b"vfk\n"
  assert await proc.wait() == 0 and proc.returncode == 0


@pytest.mark.skipif(sys.platform != "linux", reason="the vfork stub is Linux-only")
@pytest.mark.asyncio
async def test_vfork_exec_failure_raises_child_errno() -> None:
  with pytest.raises(OSError) as excinfo:
    await _spawn("/nonexistent/vfkspawn-binary", pdeathsig=True)
  assert excinfo.value.errno == 2  # ENOENT, the child's execve errno


@pytest.mark.skipif(sys.platform != "linux", reason="the vfork stub is Linux-only")
@pytest.mark.asyncio
async def test_vfork_close_fds_leaves_only_stdio() -> None:
  held = os.open("/etc/hostname", os.O_RDONLY)
  try:
    proc = await _spawn(sys.executable, "-c", "import os; print(sorted(os.listdir('/proc/self/fd')))",
                        pdeathsig=True)
    listing = await proc.stdout.readline()
    await proc.wait()
    assert str(held).encode() not in listing  # the caller's own fds never reach the child
    assert b"'0'" in listing and b"'1'" in listing and b"'2'" in listing
  finally:
    os.close(held)


_VFK_SPAWNER = (
    "import asyncio, os, sys"
    "; sys.path.insert(0, sys.argv[1])"
    "; from src.agents.backends.spawn import spawn_subprocess"
    "; proc = asyncio.run(spawn_subprocess("
    "'/bin/sleep', '30', cwd='/tmp', env=dict(os.environ),"
    "stdin=asyncio.subprocess.DEVNULL, stdout=asyncio.subprocess.PIPE,"
    "stderr=asyncio.subprocess.PIPE, limit=1024 * 1024,"
    "start_new_session=True, preexec_fn=None, pdeathsig=True))"
    "; print(proc.pid, flush=True)"
    "; proc.stdout.close()")


@pytest.mark.skipif(sys.platform != "linux", reason="the vfork stub is Linux-only")
@pytest.mark.asyncio
async def test_vfork_pdeathsig_kills_child_when_spawner_dies() -> None:
  """The piped transports' guarantee: the child cannot outlive its spawner."""
  from subprocess import PIPE, Popen

  spawner = Popen([sys.executable, "-c", _VFK_SPAWNER, os.getcwd()], stdout=PIPE, text=True)
  child_pid = int(spawner.stdout.readline().strip())
  spawner.wait()
  for _ in range(100):
    if not os.path.exists(f"/proc/{child_pid}"):
      break
    await asyncio.sleep(0.05)
  assert not os.path.exists(f"/proc/{child_pid}"), "the child survived its spawner"


@pytest.mark.skipif(sys.platform != "linux", reason="the vfork stub is Linux-only")
@pytest.mark.asyncio
async def test_vfork_killed_child_reports_signal_exit() -> None:
  proc = await _spawn(sys.executable, "-c", "import time; time.sleep(30)", pdeathsig=True)
  await asyncio.sleep(0.05)
  os.kill(proc.pid, signal.SIGKILL)
  assert await asyncio.wait_for(proc.wait(), timeout=5) == -signal.SIGKILL
  assert await proc.stdout.read() == b""
