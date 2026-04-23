"""Delayed trigger (session self-wake) manager."""

import asyncio
import os
from datetime import datetime, timedelta, timezone
from pathlib import Path

import aiofiles
import structlog

from src.api.message_utils import build_user_event
from src.core.config import CharlieBotConfig
from src.core.master_trigger import trigger_master
from src.core.models import PendingTrigger, TriggerStatus
from src.core.sessions import SessionManager
from src.core.tasks import create_logged_task

log = structlog.get_logger()

_SYS_pidfd_open = {"x86_64": 434, "aarch64": 434}


def _detect_pidfd():
  """Return (pidfd_open_callable, waitid_pidfd_callable) or (None, None).

  pidfd_open_callable(pid, flags=0) -> int, raises ProcessLookupError on ESRCH.
  waitid_pidfd_callable(fd, options) -> object with .si_pid/.si_status/.si_code
    (or None on WNOHANG no-event), raises ChildProcessError on ECHILD.
  """
  if hasattr(os, "pidfd_open") and hasattr(os, "P_PIDFD"):

    def _stdlib_pidfd_open(pid, flags=0):
      return os.pidfd_open(pid, flags)

    def _stdlib_waitid_pidfd(fd, options):
      return os.waitid(os.P_PIDFD, fd, options)

    return _stdlib_pidfd_open, _stdlib_waitid_pidfd

  import platform
  import ctypes
  import ctypes.util

  if platform.system() != "Linux":
    return None, None
  machine = platform.machine()
  if machine not in _SYS_pidfd_open:
    return None, None
  libc_path = ctypes.util.find_library("c")
  if libc_path is None:
    return None, None
  try:
    libc = ctypes.CDLL(libc_path, use_errno=True)
  except OSError:
    return None, None
  syscall_no = _SYS_pidfd_open[machine]

  # Probe: ensure kernel actually implements pidfd_open
  ctypes.set_errno(0)
  probe_fd = libc.syscall(syscall_no, os.getpid(), 0)
  if probe_fd < 0:
    return None, None
  os.close(probe_fd)

  def _ctypes_pidfd_open(pid, flags=0):
    ctypes.set_errno(0)
    fd = libc.syscall(syscall_no, pid, flags)
    if fd < 0:
      errno = ctypes.get_errno()
      if errno == 3:
        raise ProcessLookupError(errno, "no such process", pid)
      raise OSError(errno, os.strerror(errno))
    return fd

  class _Siginfo(ctypes.Structure):
    _fields_ = [
        ("si_signo", ctypes.c_int),
        ("si_errno", ctypes.c_int),
        ("si_code", ctypes.c_int),
        ("si_pid", ctypes.c_int),
        ("si_uid", ctypes.c_uint),
        ("si_status", ctypes.c_int),
        ("_pad", ctypes.c_byte * 100),
    ]

  class _WaitidResult:

    def __init__(self, si):
      self.si_pid = si.si_pid
      self.si_uid = si.si_uid
      self.si_signo = si.si_signo
      self.si_status = si.si_status
      self.si_code = si.si_code

  P_PIDFD_CONST = 3

  def _ctypes_waitid_pidfd(fd, options):
    si = _Siginfo()
    ctypes.set_errno(0)
    rc = libc.waitid(P_PIDFD_CONST, fd, ctypes.byref(si), options)
    if rc < 0:
      errno = ctypes.get_errno()
      if errno == 10:
        raise ChildProcessError(errno, os.strerror(errno))
      raise OSError(errno, os.strerror(errno))
    if si.si_pid == 0:
      return None
    return _WaitidResult(si)

  return _ctypes_pidfd_open, _ctypes_waitid_pidfd


_pidfd_open, _waitid_pidfd = _detect_pidfd()
_PIDFD_SUPPORTED = _pidfd_open is not None


class TriggerManager:
  """Manages delayed one-shot triggers that wake the master CC."""

  def __init__(self, cfg: CharlieBotConfig, session_mgr: SessionManager):
    self._cfg = cfg
    self._session_mgr = session_mgr
    self._tasks: dict[str, asyncio.Task] = {}

  async def create_trigger(
      self,
      session_id: str,
      delay_seconds: int,
      message: str,
      watch_pids: list[int] | None = None,
  ) -> PendingTrigger:
    """Create a pending trigger, persist to disk, and start the sleep task."""
    if watch_pids is not None and not _PIDFD_SUPPORTED:
      raise RuntimeError("pidfd_open unavailable: need Linux 5.3+ with kernel pidfd_open support")
    fire_at = datetime.now(timezone.utc) + timedelta(seconds=delay_seconds)
    trigger = PendingTrigger(
        session_id=session_id,
        fire_at=fire_at,
        message=message,
        watch_pids=watch_pids,
    )
    await self._save_trigger(trigger)
    self._start_task(trigger)
    log.info(
        "trigger_created",
        trigger_id=trigger.id,
        session=session_id,
        fire_at=fire_at.isoformat(),
        watch_pids=watch_pids,
    )
    return trigger

  async def list_triggers(self, session_id: str) -> list[PendingTrigger]:
    """Read all triggers for a session from disk."""
    triggers_dir = self._triggers_dir(session_id)
    if not triggers_dir.exists():
      return []
    files = await asyncio.to_thread(lambda: list(triggers_dir.glob("*.json")))
    triggers: list[PendingTrigger] = []
    for f in files:
      try:
        raw = await asyncio.to_thread(f.read_text, "utf-8")
        triggers.append(PendingTrigger.model_validate_json(raw))
      except Exception as e:
        log.warning("trigger_load_failed", path=str(f), error=str(e))
    triggers.sort(key=lambda t: t.created_at, reverse=True)
    return triggers

  async def cancel_trigger(self, session_id: str, trigger_id: str) -> None:
    """Mark a trigger as cancelled and cancel its asyncio task."""
    trigger = await self._load_trigger(session_id, trigger_id)
    if trigger.status != TriggerStatus.PENDING:
      return
    trigger.status = TriggerStatus.CANCELLED
    await self._save_trigger(trigger)
    task = self._tasks.pop(trigger_id, None)
    if task and not task.done():
      task.cancel()
    log.info("trigger_cancelled", trigger_id=trigger_id, session=session_id)

  async def recover_pending(self) -> None:
    """On startup, scan all sessions for pending triggers and restart their sleep tasks."""
    sessions_dir = self._cfg.sessions_dir
    if not sessions_dir.exists():
      return
    session_dirs = await asyncio.to_thread(lambda: [d for d in sessions_dir.iterdir() if d.is_dir()])
    for session_dir in session_dirs:
      triggers_dir = session_dir / "triggers"
      if not triggers_dir.exists():
        continue
      files = await asyncio.to_thread(lambda td=triggers_dir: list(td.glob("*.json")))
      for f in files:
        try:
          raw = await asyncio.to_thread(f.read_text, "utf-8")
          trigger = PendingTrigger.model_validate_json(raw)
        except Exception as e:
          log.warning("trigger_recovery_load_failed", path=str(f), error=str(e))
          continue
        if trigger.status == TriggerStatus.PENDING:
          self._start_task(trigger)
          log.info("trigger_recovered", trigger_id=trigger.id, session=trigger.session_id)

  def _start_task(self, trigger: PendingTrigger) -> None:
    """Start the asyncio sleep task for a trigger."""
    task = create_logged_task(self._wait_and_fire(trigger), name=f"trigger-{trigger.id[:8]}")
    self._tasks[trigger.id] = task

  async def _wait_and_fire(self, trigger: PendingTrigger) -> None:
    """Sleep until fire_at (or until all watched PIDs exit), then trigger the master agent."""
    if trigger.watch_pids:
      reason, exited, still_alive, missing = await self._wait_with_pidfd(trigger)
    else:
      now = datetime.now(timezone.utc)
      remaining = (trigger.fire_at - now).total_seconds()
      if remaining > 0:
        await asyncio.sleep(remaining)
      reason = "timeout"
      exited = []
      still_alive = []
      missing = []

    # Re-load to check for cancellation during sleep
    try:
      fresh = await self._load_trigger(trigger.session_id, trigger.id)
    except FileNotFoundError:
      log.warning("trigger_file_missing_after_sleep", trigger_id=trigger.id)
      return
    if fresh.status != TriggerStatus.PENDING:
      return

    if fresh.watch_pids:
      suffix = _format_suffix(reason, exited, still_alive, missing)
      trigger_message = f"[Scheduled trigger fired | {reason}] {fresh.message}{suffix}"
    else:
      trigger_message = f"[Scheduled trigger fired] {fresh.message}"

    await self._session_mgr.persist_and_broadcast(
        fresh.session_id,
        build_user_event(trigger_message),
    )

    # Wake the master CC
    await trigger_master(
        fresh.session_id,
        trigger_message,
        self._cfg,
        self._session_mgr,
    )

    fresh.status = TriggerStatus.FIRED
    fresh.fired_at = datetime.now(timezone.utc)
    fresh.fire_reason = reason
    await self._save_trigger(fresh)
    self._tasks.pop(trigger.id, None)
    log.info("trigger_fired", trigger_id=trigger.id, session=fresh.session_id, reason=reason)

  async def _wait_with_pidfd(
      self,
      trigger: PendingTrigger,
  ) -> tuple[str, list[tuple[int, int | None]], list[int], list[int]]:
    """Event-driven wait on pidfds. Returns (reason, exited, still_alive, missing)."""
    loop = asyncio.get_running_loop()
    pidfds: dict[int, int] = {}  # fd -> pid
    exited: list[tuple[int, int | None]] = []  # (pid, exit_status_or_None)

    missing: list[int] = []
    for pid in trigger.watch_pids or []:
      try:
        fd = _pidfd_open(pid)
      except ProcessLookupError:
        missing.append(pid)
        continue
      pidfds[fd] = pid

    if missing:
      for fd in list(pidfds):
        try:
          os.close(fd)
        except OSError:
          pass
      return "pid_gone", [], [], missing

    done = asyncio.Event()

    def on_ready(fd: int) -> None:
      pid = pidfds.pop(fd, None)
      if pid is None:
        return
      try:
        loop.remove_reader(fd)
      except Exception:
        pass
      status: int | None = None
      try:
        info = _waitid_pidfd(fd, os.WEXITED | os.WNOHANG)
        if info is not None:
          status = info.si_status
      except (ChildProcessError, OSError):
        status = None  # non-child; cannot retrieve status
      try:
        os.close(fd)
      except OSError:
        pass
      exited.append((pid, status))
      if not pidfds:
        done.set()

    for fd in list(pidfds):
      loop.add_reader(fd, on_ready, fd)

    now = datetime.now(timezone.utc)
    remaining = (trigger.fire_at - now).total_seconds()
    reason: str
    try:
      await asyncio.wait_for(done.wait(), timeout=max(0.0, remaining))
      reason = "pid_exit"
    except asyncio.TimeoutError:
      reason = "timeout"
    finally:
      # Cleanup any remaining fds (e.g. timeout case, or cancellation)
      for fd in list(pidfds):
        try:
          loop.remove_reader(fd)
        except Exception:
          pass
        try:
          os.close(fd)
        except OSError:
          pass

    still_alive = list(pidfds.values())
    return reason, exited, still_alive, []

  def _triggers_dir(self, session_id: str) -> Path:
    return self._cfg.sessions_dir / session_id / "triggers"

  def _trigger_path(self, session_id: str, trigger_id: str) -> Path:
    return self._triggers_dir(session_id) / f"{trigger_id}.json"

  async def _save_trigger(self, trigger: PendingTrigger) -> None:
    path = self._trigger_path(trigger.session_id, trigger.id)
    path.parent.mkdir(parents=True, exist_ok=True)
    async with aiofiles.open(path, "w") as f:
      await f.write(trigger.model_dump_json(indent=2))

  async def _load_trigger(self, session_id: str, trigger_id: str) -> PendingTrigger:
    path = self._trigger_path(session_id, trigger_id)
    async with aiofiles.open(path, "r") as f:
      raw = await f.read()
    return PendingTrigger.model_validate_json(raw)


def _format_suffix(
    reason: str,
    exited: list[tuple[int, int | None]],
    still_alive: list[int],
    missing: list[int],
) -> str:
  """Build the message suffix describing PID outcomes for a fired trigger."""
  if reason == "pid_gone":
    pids = ", ".join(str(p) for p in missing)
    return f" (pid_gone: {pids})"
  if reason == "pid_exit":
    parts = ", ".join(f"{pid}={'unknown' if s is None else s}" for pid, s in exited)
    return f" (exited: {parts})"
  # timeout
  exited_parts = ", ".join(f"{pid}={'unknown' if s is None else s}" for pid, s in exited)
  alive_parts = ", ".join(str(p) for p in still_alive)
  if exited_parts:
    return f" (exited: {exited_parts}; still alive: {alive_parts})"
  return f" (still alive: {alive_parts})"
