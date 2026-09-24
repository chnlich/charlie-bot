"""Off-loop subprocess spawn for the backend launch family.

``asyncio.create_subprocess_exec`` forks the calling process synchronously on
the event loop, and the fork's page-table copy scales with the forking
process's resident set (~55 us/MB measured on this host) — a multi-GB server
stalls every concurrent request and WebSocket for ~0.1-0.2 s on each
master/worker launch. This module parks the fork+exec handshake on a worker
thread and wires the child's pipes onto the caller's loop afterwards; the
preexec composition (nice, session cgroup, pdeathsig) still runs in the child
after fork exactly as ``create_subprocess_exec`` ran it, so the child-side
semantics are unchanged.

The returned handle mirrors the ``asyncio.subprocess.Process`` surface the
backend launch path reads: ``pid``, ``stdin`` (StreamWriter | None),
``stdout``/``stderr`` (StreamReader | None), ``returncode``, ``wait()``.
stdout/stderr redirected to file descriptors stay unwired (None), matching the
asyncio transport.
"""

from __future__ import annotations

import asyncio
import functools
import os
import subprocess
import threading
from collections.abc import Callable


def _reap(
    popen: subprocess.Popen | _VforkHandle, loop: asyncio.AbstractEventLoop, exit_future: asyncio.Future[int]) -> None:
  """Park on waitpid until the child exits, then resolve the exit future.

  The reaper thread owns this child's status: no other code path waitpid()s
  it, so ``returncode`` reads race nothing. Daemon, so a child outliving the
  process never blocks interpreter exit (the old child-watcher threads were
  daemons too).
  """
  code = popen.wait()
  loop.call_soon_threadsafe(exit_future.set_result, code)


class _VforkHandle:
  """Minimal Popen-twin over a vfork-spawned pid: pid, returncode, wait()."""

  def __init__(self, pid: int) -> None:
    self.pid = pid
    self.returncode: int | None = None

  def wait(self) -> int:
    if self.returncode is None:
      _, status = os.waitpid(self.pid, 0)
      self.returncode = os.waitstatus_to_exitcode(status)
    return self.returncode


def _vfork_exec(argv: list[str], cwd: str | None, env: dict, stdin_fd: int, stdout_fd: int, stderr_fd: int) -> int:
  """Run the clone(CLONE_VM|CLONE_VFORK) handshake, returning the child's pid.

  Lazy import: the compiled module exists where the package was installed with
  a C toolchain, so a missing build surfaces here, at the first pdeathsig
  spawn, instead of at startup.
  """
  from src.agents.backends._vfkspawn import spawn as vfork_spawn

  env_items = [f"{key}={value}" for key, value in env.items()]
  return vfork_spawn(argv, env_items, cwd, stdin_fd, stdout_fd, stderr_fd, os.getpid())


class SpawnedProcess:
  """Thread-spawned Popen with its pipes riding the caller's event loop."""

  def __init__(
      self,
      popen: subprocess.Popen | _VforkHandle,
      *,
      stdin: asyncio.StreamWriter | None,
      stdout: asyncio.StreamReader | None,
      stderr: asyncio.StreamReader | None,
      loop: asyncio.AbstractEventLoop,
  ) -> None:
    self._popen = popen
    self.stdin = stdin
    self.stdout = stdout
    self.stderr = stderr
    self.pid = popen.pid
    self._exit: asyncio.Future[int] = loop.create_future()
    self._reaper = threading.Thread(
        target=_reap, args=(popen, loop, self._exit), name=f"spawn-reap-{popen.pid}", daemon=True)
    self._reaper.start()

  @property
  def returncode(self) -> int | None:
    return self._popen.returncode

  async def wait(self) -> int:
    # Shielded: one waiter's wait_for timeout cancels only its own await —
    # the shared exit future stays pending for the reaper to resolve and the
    # next waiter (the cleanup path's SIGKILL escalation) reads the real exit.
    return await asyncio.shield(self._exit)


async def _wire_reader(pipe: object, *, limit: int, loop: asyncio.AbstractEventLoop) -> asyncio.StreamReader:
  reader = asyncio.StreamReader(limit=limit)
  protocol = asyncio.StreamReaderProtocol(reader)
  await loop.connect_read_pipe(lambda: protocol, pipe)
  return reader


class _StdinPipeProtocol(asyncio.streams.FlowControlMixin):
  """FlowControlMixin with the close-waiter the StreamWriter contract needs.

  The subprocess stdin's own protocol subclasses FlowControlMixin the same way;
  the bare mixin's ``wait_closed`` raises NotImplementedError.
  """

  def __init__(self, loop: asyncio.AbstractEventLoop | None = None) -> None:
    super().__init__(loop=loop)
    self._closed = (loop or asyncio.get_event_loop()).create_future()

  def connection_lost(self, exc: Exception | None) -> None:
    super().connection_lost(exc)
    if not self._closed.done():
      if exc is None:
        self._closed.set_result(None)
      else:
        self._closed.set_exception(exc)

  def _get_close_waiter(self, stream: object) -> asyncio.Future:
    return self._closed


async def _wire_writer(pipe: object, loop: asyncio.AbstractEventLoop) -> asyncio.StreamWriter:
  transport, protocol = await loop.connect_write_pipe(_StdinPipeProtocol, pipe)
  return asyncio.StreamWriter(transport, protocol, None, loop)


async def spawn_subprocess(
    *cmd: str,
    cwd: str | None = None,
    env: dict,
    stdin: int,
    stdout: int,
    stderr: int,
    limit: int,
    start_new_session: bool = True,
    preexec_fn: Callable[[], None] | None = None,
    pdeathsig: bool = False,
) -> SpawnedProcess:
  """Spawn *cmd* with the fork+exec handshake off the event loop.

  Call-shape twin of ``asyncio.create_subprocess_exec`` for the backend launch
  family: same argument names, same child-side preexec semantics, the fork
  itself parked on a worker thread. ``bufsize=0`` keeps the piped fds raw for
  the selector-driven transports, the same unbuffered shape the asyncio
  subprocess transport wires.

  ``pdeathsig=True`` is the piped family's shape: stdin devnull, both outputs
  piped, a new session, no preexec hook — the child-side setup is the vfork
  stub's fixed sequence, and the fork rides clone(CLONE_VM|CLONE_VFORK) so the
  page-table copy the preexec fork pays never happens. The nice raise and the
  session cgroup move stay parent-side at the caller
  (:meth:`AgentBackend._apply_turn_tree_limits`), the run()'s raw-log shape.
  """
  if pdeathsig:
    # The stub's child-side setup is a fixed sequence: the shape and the hook
    # absence are the seam's contract, not caller options.
    assert stdin == subprocess.DEVNULL and stdout == subprocess.PIPE and stderr == subprocess.PIPE
    assert start_new_session and preexec_fn is None
    loop = asyncio.get_running_loop()
    devnull = os.open(os.devnull, os.O_RDONLY)
    out_r, out_w = os.pipe()
    err_r, err_w = os.pipe()
    try:
      pid = await loop.run_in_executor(
          None, functools.partial(_vfork_exec, list(cmd), cwd, env, devnull, out_w, err_w))
    except BaseException:
      for fd in (devnull, out_w, err_w, out_r, err_r):
        os.close(fd)
      raise
    for fd in (devnull, out_w, err_w):
      os.close(fd)
    stdout_stream = await _wire_reader(os.fdopen(out_r, "rb", buffering=0), limit=limit, loop=loop)
    stderr_stream = await _wire_reader(os.fdopen(err_r, "rb", buffering=0), limit=limit, loop=loop)
    return SpawnedProcess(_VforkHandle(pid), stdin=None, stdout=stdout_stream, stderr=stderr_stream, loop=loop)
  loop = asyncio.get_running_loop()
  popen = await loop.run_in_executor(
      None,
      functools.partial(
          subprocess.Popen,
          list(cmd),
          cwd=cwd,
          env=env,
          stdin=stdin,
          stdout=stdout,
          stderr=stderr,
          bufsize=0,
          start_new_session=start_new_session,
          preexec_fn=preexec_fn,
      ),
  )
  stdin_stream = await _wire_writer(popen.stdin, loop) if popen.stdin is not None else None
  stdout_stream = await _wire_reader(popen.stdout, limit=limit, loop=loop) if popen.stdout is not None else None
  stderr_stream = await _wire_reader(popen.stderr, limit=limit, loop=loop) if popen.stderr is not None else None
  return SpawnedProcess(popen, stdin=stdin_stream, stdout=stdout_stream, stderr=stderr_stream, loop=loop)
