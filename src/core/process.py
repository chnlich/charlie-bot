"""Process management utilities."""

import asyncio
import os
import signal
import time
from collections.abc import Callable

import structlog

from src.core.timeouts import (
  KILL_ESCALATION_GRACE_SECONDS,
  KILL_ESCALATION_POLL_SECONDS,
)

log = structlog.get_logger()


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
