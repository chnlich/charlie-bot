"""Delayed trigger (session self-wake) manager."""

import asyncio
import json
import os
import random
from datetime import datetime, timedelta, timezone
from pathlib import Path

import aiofiles
import structlog

from src.api.message_utils import build_user_event
from src.core.config import CharlieBotConfig
from src.core.master_trigger import trigger_master
from src.core.models import PendingTrigger, TriggerStatus, WatchTarget
from src.core.sessions import SessionManager
from src.core.tasks import create_logged_task

log = structlog.get_logger()

_SYS_pidfd_open = {"x86_64": 434, "aarch64": 434}

# Backoff intervals (seconds) for the remote ssh probe loop.
_REMOTE_PROBE_INTERVALS = [10, 20, 40, 80, 160, 320]
_REMOTE_PROBE_PLATEAU = 600
_REMOTE_PROBE_NOISE_MAX = 10  # uniform random 0..10s added to each interval

# Per-probe ssh subprocess timeouts (locked, no flag).
_SSH_CONNECT_TIMEOUT = 10  # ssh -o ConnectTimeout=10
_SSH_OVERALL_TIMEOUT = 60.0  # asyncio.wait_for timeout wrapping the subprocess


class RemoteVerifyError(Exception):
  """Raised when verify-on-create fails for a remote watch target."""


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


# ---------------------------------------------------------------------------
# Remote probe via ssh
# ---------------------------------------------------------------------------


async def _ssh_probe_pid(host: str, pid: int) -> tuple[str, str]:
  """Probe a single (host, pid) via ssh `kill -0`.

  Returns (status, raw_output) where status is one of:
    - "ALIVE": pid exists on host
    - "DEAD": pid does not exist on host
    - "ERROR": ssh failed / timed out / unexpected output (transient)
  """
  cmd = [
      "ssh",
      "-o",
      "BatchMode=yes",
      "-o",
      f"ConnectTimeout={_SSH_CONNECT_TIMEOUT}",
      host,
      f"kill -0 {pid} 2>&1 && echo ALIVE || echo DEAD",
  ]
  try:
    proc = await asyncio.create_subprocess_exec(
        *cmd,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )
  except OSError as e:
    return "ERROR", f"spawn failed: {e}"

  try:
    stdout_b, stderr_b = await asyncio.wait_for(proc.communicate(), timeout=_SSH_OVERALL_TIMEOUT)
  except asyncio.TimeoutError:
    proc.kill()
    try:
      await proc.wait()
    except Exception as e:
      log.debug("ssh_probe_wait_after_kill_failed", host=host, pid=pid, error=str(e))
    return "ERROR", f"ssh timeout after {_SSH_OVERALL_TIMEOUT}s"

  stdout = stdout_b.decode("utf-8", errors="replace")
  stderr = stderr_b.decode("utf-8", errors="replace")
  raw = stdout + stderr
  last_line = stdout.strip().splitlines()[-1].strip() if stdout.strip() else ""
  if last_line == "ALIVE":
    return "ALIVE", raw
  if last_line == "DEAD":
    return "DEAD", raw
  combined = (stdout + stderr).strip() or f"ssh exit {proc.returncode}"
  return "ERROR", combined


# ---------------------------------------------------------------------------
# Schema migration: legacy `watch_pids: list[int]` -> `watch_targets: list[WatchTarget]`
# ---------------------------------------------------------------------------


def _migrate_legacy_watch_pids(raw_text: str) -> tuple[PendingTrigger, bool]:
  """Parse a trigger JSON string; if it has legacy `watch_pids`, convert in-memory.

  Returns (trigger, migrated). When `migrated` is True, the file should be
  rewritten by the caller in the new schema. The new schema treats every
  legacy pid as local (host=None).
  """
  data = json.loads(raw_text)
  migrated = False
  if "watch_pids" in data:
    legacy = data.pop("watch_pids")
    if "watch_targets" not in data:
      if legacy:
        data["watch_targets"] = [{"host": None, "pid": int(p)} for p in legacy]
      else:
        data["watch_targets"] = []
      migrated = True
  return PendingTrigger.model_validate(data), migrated


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
      watch_targets: list[WatchTarget] | None = None,
  ) -> PendingTrigger:
    """Create a pending trigger, persist to disk, and start the sleep task."""
    targets = list(watch_targets or [])

    has_local = any(t.host is None for t in targets)
    has_remote = any(t.host is not None for t in targets)
    if has_local and has_remote:
      raise RuntimeError("watch_targets must be all local or all remote, not mixed")
    if has_local and not _PIDFD_SUPPORTED:
      raise RuntimeError("pidfd_open unavailable: need Linux 5.3+ with kernel pidfd_open support")

    if has_remote:
      await self._verify_remote_targets(targets)

    fire_at = datetime.now(timezone.utc) + timedelta(seconds=delay_seconds)
    trigger = PendingTrigger(
        session_id=session_id,
        fire_at=fire_at,
        message=message,
        watch_targets=targets,
    )
    await self._save_trigger(trigger)
    self._start_task(trigger)
    log.info(
        "trigger_created",
        trigger_id=trigger.id,
        session=session_id,
        fire_at=fire_at.isoformat(),
        watch_targets=[t.model_dump() for t in targets],
    )
    return trigger

  async def _verify_remote_targets(self, targets: list[WatchTarget]) -> None:
    """Probe each remote target once before persisting; reject if any not ALIVE."""
    results = await asyncio.gather(
        *[_ssh_probe_pid(t.host, t.pid) for t in targets if t.host is not None]
    )
    bad: list[str] = []
    for t, (status, raw) in zip(
        [t for t in targets if t.host is not None],
        results,
    ):
      if status != "ALIVE":
        bad.append(f"{t.host}:{t.pid} -> {status} ({raw.strip()!r})")
    if bad:
      raise RemoteVerifyError(
          "verify-on-create failed for remote watch target(s): " + "; ".join(bad)
      )

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
        trigger, _ = _migrate_legacy_watch_pids(raw)
        triggers.append(trigger)
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
          trigger, migrated = _migrate_legacy_watch_pids(raw)
        except Exception as e:
          log.warning("trigger_recovery_load_failed", path=str(f), error=str(e))
          continue
        if migrated:
          # Rewrite the legacy file once with the new schema.
          await asyncio.to_thread(
              f.write_text,
              trigger.model_dump_json(indent=2),
              "utf-8",
          )
          log.info("trigger_schema_migrated", path=str(f), trigger_id=trigger.id)
        if trigger.status == TriggerStatus.PENDING:
          self._start_task(trigger)
          log.info("trigger_recovered", trigger_id=trigger.id, session=trigger.session_id)

  def _start_task(self, trigger: PendingTrigger) -> None:
    """Start the asyncio sleep task for a trigger."""
    task = create_logged_task(self._wait_and_fire(trigger), name=f"trigger-{trigger.id[:8]}")
    self._tasks[trigger.id] = task

  async def _wait_and_fire(self, trigger: PendingTrigger) -> None:
    """Sleep until fire_at (or until all watched PIDs exit), then trigger the master agent."""
    targets = trigger.watch_targets
    has_local = any(t.host is None for t in targets)
    has_remote = any(t.host is not None for t in targets)

    if targets and has_local and not has_remote:
      reason, exited, still_alive, missing = await self._wait_with_pidfd(trigger)
    elif targets and has_remote and not has_local:
      reason, exited, still_alive, missing = await self._wait_with_remote_probe(trigger)
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

    if fresh.watch_targets:
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
  ) -> tuple[str, list[tuple[str, int | None]], list[str], list[str]]:
    """Event-driven wait on pidfds. Returns (reason, exited, still_alive, missing).

    `exited` items are (label, status) where label is the local-pid string.
    `still_alive` and `missing` are lists of pid labels (strings).
    """
    loop = asyncio.get_running_loop()
    pidfds: dict[int, int] = {}  # fd -> pid
    exited: list[tuple[str, int | None]] = []  # (label, exit_status_or_None)

    missing: list[str] = []
    local_pids = [t.pid for t in trigger.watch_targets if t.host is None]
    for pid in local_pids:
      try:
        fd = _pidfd_open(pid)
      except ProcessLookupError:
        missing.append(str(pid))
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
      exited.append((str(pid), status))
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

    still_alive = [str(p) for p in pidfds.values()]
    return reason, exited, still_alive, []

  async def _wait_with_remote_probe(
      self,
      trigger: PendingTrigger,
  ) -> tuple[str, list[tuple[str, int | None]], list[str], list[str]]:
    """Polling wait via ssh probes with backoff.

    Returns (reason, exited, still_alive, missing) using ``host:pid`` labels.
    Verify-on-create has already confirmed every PID was ALIVE at create time,
    so `missing` is always empty here.
    """
    remaining: dict[str, set[int]] = {}
    for t in trigger.watch_targets:
      if t.host is None:
        continue
      remaining.setdefault(t.host, set()).add(t.pid)

    exited: list[tuple[str, int | None]] = []
    step = 0
    reason = "timeout"

    while True:
      now = datetime.now(timezone.utc)
      time_to_fire = (trigger.fire_at - now).total_seconds()
      if time_to_fire <= 0:
        break

      base = _REMOTE_PROBE_INTERVALS[step] if step < len(_REMOTE_PROBE_INTERVALS) else _REMOTE_PROBE_PLATEAU
      sleep_for = base + random.uniform(0, _REMOTE_PROBE_NOISE_MAX)
      await asyncio.sleep(min(sleep_for, time_to_fire))

      now = datetime.now(timezone.utc)
      if (trigger.fire_at - now).total_seconds() <= 0:
        break

      probes: list[tuple[str, int]] = []
      for host, pids in remaining.items():
        for pid in pids:
          probes.append((host, pid))
      results = await asyncio.gather(*[_ssh_probe_pid(h, p) for h, p in probes])
      for (host, pid), (status, raw) in zip(probes, results):
        if status == "DEAD":
          remaining[host].discard(pid)
          if not remaining[host]:
            del remaining[host]
          exited.append((f"{host}:{pid}", None))
        elif status == "ALIVE":
          continue
        else:
          # transient error / timeout — keep watching
          log.debug(
              "remote_probe_transient_error",
              trigger_id=trigger.id,
              host=host,
              pid=pid,
              raw=raw.strip(),
          )

      step += 1
      if not remaining:
        reason = "pid_exit"
        break

    still_alive: list[str] = []
    for host, pids in remaining.items():
      for pid in pids:
        still_alive.append(f"{host}:{pid}")

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
    trigger, _ = _migrate_legacy_watch_pids(raw)
    return trigger


def _format_suffix(
    reason: str,
    exited: list[tuple[str, int | None]],
    still_alive: list[str],
    missing: list[str],
) -> str:
  """Build the message suffix describing PID outcomes for a fired trigger.

  Labels in `exited`, `still_alive`, and `missing` are pre-formatted strings:
  `"1234"` for local PIDs and `"host:5678"` for remote PIDs.
  """
  if reason == "pid_gone":
    pids = ", ".join(missing)
    return f" (pid_gone: {pids})"
  if reason == "pid_exit":
    parts = ", ".join(_format_exited_part(label, s) for label, s in exited)
    return f" (exited: {parts})"
  # timeout
  exited_parts = ", ".join(_format_exited_part(label, s) for label, s in exited)
  alive_parts = ", ".join(still_alive)
  if exited_parts:
    return f" (exited: {exited_parts}; still alive: {alive_parts})"
  return f" (still alive: {alive_parts})"


def _format_exited_part(label: str, status: int | None) -> str:
  """Format a single exited entry. Remote labels (host:pid) omit the status field."""
  if ":" in label:
    return label
  return f"{label}={'unknown' if status is None else status}"
