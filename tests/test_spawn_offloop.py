"""Off-loop spawn contracts for src/runtime/agent_process/spawn.py.

The backend launch family (master turns, workers, one-shots) spawns through
``spawn_subprocess``: the fork+exec handshake parks on a worker thread so the
server's multi-GB resident set never prices its page-table copy onto the event
loop, while the preexec composition still runs in the child exactly as
``asyncio.create_subprocess_exec`` ran it. The off-loop property is pinned by
blocking the child inside its preexec and asserting the loop keeps ticking.
"""

from __future__ import annotations

import asyncio
import contextlib
import io
import itertools
import os
import pathlib
import signal
import subprocess
import sys
import threading
import time

import pytest

from src.runtime.agent_process import spawn

LIMIT = 1024 * 1024

_PY_PRINT = (
    "import sys"
    "; sys.stdout.write('line-one\\nline-two\\n'); sys.stdout.flush()"
    "; sys.stderr.write('err-one\\n'); sys.stderr.flush()")


async def _spawn(*args: str, **kwargs: object) -> spawn.SpawnedProcess:
  defaults: dict = {
      "cwd": "/tmp",
      "env": dict(os.environ),
      "stdin": asyncio.subprocess.DEVNULL,
      "stdout": asyncio.subprocess.PIPE,
      "stderr": asyncio.subprocess.PIPE,
      "limit": LIMIT,
      "start_new_session": True,
      "preexec_fn": None,
  }
  defaults.update(kwargs)
  return await spawn.spawn_subprocess(*args, **defaults)


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
async def test_killed_child_reports_signal_exit() -> None:
  proc = await _spawn(sys.executable, "-c", "import time; time.sleep(30)")
  await asyncio.sleep(0.05)
  os.kill(proc.pid, signal.SIGKILL)
  assert await asyncio.wait_for(proc.wait(), timeout=5) == -signal.SIGKILL


_VFK_SPAWNER = (
    "import asyncio, os, sys"
    "; sys.path.insert(0, sys.argv[1])"
    "; from src.runtime.agent_process.spawn import spawn_subprocess"
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
  spawner = subprocess.Popen([sys.executable, "-c", _VFK_SPAWNER, os.getcwd()], stdout=subprocess.PIPE, text=True)
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


@pytest.mark.skipif(sys.platform != "linux", reason="the vfork stub is Linux-only")
def test_vfork_slow_exec_does_not_stop_other_threads(monkeypatch: pytest.MonkeyPatch) -> None:
  """The stub's clone-through-report-read wait releases the GIL.

  A child that sits before execve held the GIL on the pre-fix stub — a probe
  with an strace-injected 3 s exec delay stopped every other Python thread of
  the server for 3.005 s — so a ticker thread here must keep ticking through
  the spawn. The stall rides the stub's test hook
  (``_VFORKSPAWN_TEST_EXEC_DELAY_MS``, a nanosleep in child_main) instead of
  the probe's ptrace injection.
  """
  monkeypatch.setenv("_VFORKSPAWN_TEST_EXEC_DELAY_MS", "1000")
  ticks: list[float] = []
  stop = threading.Event()

  def ticker() -> None:
    while not stop.is_set():
      ticks.append(time.monotonic())
      time.sleep(0.01)

  tick_thread = threading.Thread(target=ticker)
  tick_thread.start()
  try:
    t0 = time.monotonic()
    devnull = os.open(os.devnull, os.O_RDWR)
    try:
      pid = spawn.vfkspawn.spawn(
          ["/bin/true"], [f"PATH={os.environ['PATH']}"], None, devnull, devnull, devnull, os.getpid())
      os.waitpid(pid, 0)
    finally:
      os.close(devnull)
    spawn_s = time.monotonic() - t0
  finally:
    stop.set()
    tick_thread.join()
  assert spawn_s >= 0.9, f"the child never blocked before execve; the hook did not fire (spawn took {spawn_s:.3f}s)"
  inside = [t for t in ticks if t0 - 0.05 <= t <= t0 + spawn_s + 0.05]
  max_gap = max((b - a for a, b in itertools.pairwise(inside)), default=float("nan"))
  assert max_gap < 0.5, f"the other threads stalled {max_gap:.3f}s during the spawn"


class _FdReuseProbe(io.FileIO):
  """Pipe file object whose close() reenacts the fd-reuse race a double close loses.

  Handed to the spawn pipe wiring, its close() opens a scratch file until the fd
  table hands that file the pipe's own number — possible only when the fd was
  freed behind the file object's back, uvloop's libuv-then-fileobj double close —
  then runs the real close, then writes the scratch file. The write's outcome
  travels on ``write_error`` because the loop routes exceptions from transport
  close callbacks to its error handler, not to the caller.
  """

  def __init__(self, fd: int, mode: str, victim_path: str) -> None:
    super().__init__(fd, mode)
    self._victim_path = victim_path
    self.write_error: OSError | None = None

  def close(self) -> None:
    if self.closed:
      return
    fd = self.fileno()
    allocated: list[int] = []
    try:
      victim = -1
      for _ in range(fd + 2):
        victim = os.open(self._victim_path, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)
        allocated.append(victim)
        if victim >= fd:
          break
      super().close()
      try:
        os.write(victim, b'{"probe":true}\n')
      except OSError as exc:
        self.write_error = exc
    finally:
      for allocated_fd in allocated:
        with contextlib.suppress(OSError):
          os.close(allocated_fd)


async def _close_one_piped_transport(direction: str, victim_path: str) -> _FdReuseProbe:
  """Wire one pipe direction through spawn on the running loop, then have the loop close it."""
  loop = asyncio.get_running_loop()
  read_fd, write_fd = os.pipe()
  if direction == "read":
    probe = _FdReuseProbe(read_fd, "rb", victim_path)
    reader = await spawn._wire_reader(probe, limit=LIMIT, loop=loop)
    os.close(write_fd)  # EOF: the read transport closes its pipe in response.
    assert await reader.read() == b""
    await asyncio.sleep(0.05)
  else:
    probe = _FdReuseProbe(write_fd, "wb", victim_path)
    writer = await spawn._wire_writer(probe, loop)
    writer.close()
    await writer.wait_closed()
    os.close(read_fd)
  return probe


@pytest.mark.parametrize("direction", ["read", "write"])
def test_pipe_close_never_kills_a_reused_fd(tmp_path: pathlib.Path, direction: str) -> None:
  """The service event loop closes a subprocess pipe fd exactly once.

  uvloop frees the fd in libuv and then calls the file object's close() a second
  time; a file opened between the two closes inherits the number and dies with
  EBADF (https://github.com/MagicStack/uvloop/issues/763). The probe folds that
  interleaving into close() itself. The probe runs on the stdlib loop, which closes each fd once.
  """
  with asyncio.Runner() as runner:
    probe = runner.run(_close_one_piped_transport(direction, str(tmp_path / "victim.jsonl")))
  # The stdlib loop closes the pipe inside the scenario (EOF/close replies); a
  # loop that defers the close to its teardown — uvloop's shape — has still run
  # it by Runner exit. The closed flag is the receipt that the pipe really closed.
  assert probe.closed
  assert probe.write_error is None, f"the reused fd died: {probe.write_error!r}"
