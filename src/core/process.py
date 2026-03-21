"""Process management utilities."""

import os
import signal

import structlog

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
