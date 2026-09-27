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
import subprocess
import sys

import pytest

from src.agents.backends.spawn import SpawnedProcess, spawn_subprocess

LIMIT = 1024 * 1024

_PY_PRINT = (
    "import sys"
    "; sys.stdout.write('line-one\\nline-two\\n'); sys.stdout.flush()"
    "; sys.stderr.write('err-one\\n'); sys.stderr.flush()")


async def _spawn(*args: str, **kwargs: object) -> SpawnedProcess:
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
async def test_killed_child_reports_signal_exit() -> None:
  proc = await _spawn(sys.executable, "-c", "import time; time.sleep(30)")
  await asyncio.sleep(0.05)
  os.kill(proc.pid, signal.SIGKILL)
  assert await asyncio.wait_for(proc.wait(), timeout=5) == -signal.SIGKILL


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
