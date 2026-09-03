"""Process management utilities."""

import asyncio
import contextlib
import ctypes
import os
import signal
import sys
import time
from collections.abc import Callable, Coroutine
from typing import Any, TypeVar

import structlog

from src.core.timeouts import (
    KILL_ESCALATION_GRACE_SECONDS,
    KILL_ESCALATION_POLL_SECONDS,
)

log = structlog.get_logger()

# Named TypeVar instead of PEP 695 ``wait_or_kill_group[T]``: yapf's pinned
# lib2to3 parser rejects PEP 695 type-parameter lists, and the inline form
# makes the whole tree unparseable to ``yapf -r``.
_T = TypeVar("_T")

# linux/prctl.h option number; not exposed by the stdlib.
_PR_SET_PDEATHSIG = 1

# Resolve libc and the prctl symbol once, at import time: the child-side preexec
# callable runs between fork and exec, where locks other threads held at fork
# time may be in any state, so its path does the pre-resolved syscall and
# nothing else.
if sys.platform == "linux":
  _libc = ctypes.CDLL("libc.so.6", use_errno=True)
  _prctl = _libc.prctl
  _prctl.argtypes = [ctypes.c_int, ctypes.c_ulong, ctypes.c_ulong, ctypes.c_ulong, ctypes.c_ulong]
  _prctl.restype = ctypes.c_int
else:
  _prctl = None


def kill_process_group(pid: int, sig: signal.Signals = signal.SIGTERM) -> bool:
  """Send a signal to the process group of *pid*.

  Returns True if the signal was delivered, False if the process was
  already gone or the signal could not be sent.
  """
  try:
    os.killpg(os.getpgid(pid), sig)
    return True
  except (ProcessLookupError, PermissionError):
    log.debug("kill_pg_gone", pid=pid, sig=sig.name)
    return False
  except Exception as err:
    log.debug("kill_pg_failed", pid=pid, sig=sig.name, error=str(err))
    return False


async def kill_group_escalating(pid: int, is_alive: Callable[[], bool]) -> None:
  """SIGTERM *pid*'s process group; SIGKILL it when it outlives the grace window.

  *is_alive* is the caller's liveness proof for the group; the SIGKILL fires only
  when it still returns True after the grace, so a stale probe never authorizes a kill.
  """
  kill_process_group(pid, signal.SIGTERM)
  deadline = time.monotonic() + KILL_ESCALATION_GRACE_SECONDS
  while is_alive() and time.monotonic() < deadline:
    await asyncio.sleep(KILL_ESCALATION_POLL_SECONDS)
  if is_alive():
    kill_process_group(pid, signal.SIGKILL)


async def wait_or_kill_group(
    coro: Coroutine[Any, Any, _T], timeout: float, pid: int, stderr_task: asyncio.Task[bytes]) -> _T:
  """Await *coro* for at most *timeout* seconds; cancel and drain *stderr_task* on every exit.

  On timeout the process group of *pid* is SIGKILLed before the TimeoutError
  propagates. The drain (cancel, then await while suppressing CancelledError)
  guarantees the stderr task never outlives the caller.
  """
  try:
    return await asyncio.wait_for(coro, timeout)
  except TimeoutError:
    kill_process_group(pid, signal.SIGKILL)
    raise
  finally:
    stderr_task.cancel()
    with contextlib.suppress(asyncio.CancelledError):
      await stderr_task


def _pdeathsig_should_self_kill(parent_pid: int, observed_ppid: int) -> bool:
  """Post-prctl re-check decision: self-kill iff the observed parent is not the captured one.

  A mismatch means the parent died inside the fork→prctl window, so the just-registered
  PR_SET_PDEATHSIG was bound against a dead parent and will never fire; the child must
  then reap itself rather than orphan.
  """
  return observed_ppid != parent_pid


def make_pdeathsig_kill_preexec() -> Callable[[], None] | None:
  """preexec_fn binding the spawned child to this process's death, or None off Linux.

  The callable runs in the child between fork and exec: it registers
  PR_SET_PDEATHSIG=SIGKILL through the import-time-resolved libc handle, then closes
  the fork→prctl race with a getppid re-check (see ``_pdeathsig_should_self_kill``).
  SIGKILL because the triggering scenario has the parent already gone: the child's
  output has no consumer and it persists no state needing a graceful shutdown.
  Piped-transport backend children are bound this way; covered (raw-log) transports
  must NOT be — they are designed to survive parent death and be re-attached.
  """
  if sys.platform != "linux":
    return None
  parent_pid = os.getpid()

  def _preexec() -> None:
    if _prctl(_PR_SET_PDEATHSIG, signal.SIGKILL, 0, 0, 0) != 0:
      raise OSError(ctypes.get_errno(), "prctl(PR_SET_PDEATHSIG) failed")
    observed_ppid = os.getppid()
    if _pdeathsig_should_self_kill(parent_pid, observed_ppid):
      os.kill(os.getpid(), signal.SIGKILL)

  return _preexec
