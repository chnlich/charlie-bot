"""Delayed trigger (session self-wake) manager."""

import asyncio
import contextlib
import json
import os
import random
import shutil
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any

import aiofiles
import structlog

from src.api.message_utils import build_scheduled_trigger_event
from src.core.config import CharlieBotConfig, get_config
from src.core.master_trigger import trigger_master
from src.core.models import (
  LocalPid,
  PendingTrigger,
  RemotePid,
  SessionStatus,
  SlurmJob,
  TriggerStatus,
  WatchKind,
  WatchTarget,
)
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

# SLURM watch (sacct polling).
_SACCT_POLL_INTERVAL = 30  # seconds between sacct probes
# A remote sacct host that answers nothing for this long is treated as unobservable: the
# group stops waiting and reports itself so the master wakes up instead of going blind.
_REMOTE_SACCT_UNREACHABLE_GRACE = 900  # seconds
# Non-terminal job states: the job is still in flight, keep polling.
_SLURM_ACTIVE_STATES = frozenset({
    "PENDING",
    "RUNNING",
    "CONFIGURING",
    "COMPLETING",
    "REQUEUED",
    "RESIZING",
    "SUSPENDED",
})
# Terminal job states: the job has stopped, capture State + ExitCode. Any state
# string in neither set is unknown (logged, treated as not-finished).
_SLURM_TERMINAL_STATES = frozenset({
    "COMPLETED",
    "FAILED",
    "CANCELLED",
    "TIMEOUT",
    "OUT_OF_MEMORY",
    "NODE_FAIL",
    "BOOT_FAIL",
    "DEADLINE",
    "PREEMPTED",
    "REVOKED",
    "SPECIAL_EXIT",
})


class RemoteVerifyError(Exception):
  """Raised when verify-on-create fails for a remote watch target."""


def _detect_pidfd():
  """Return (pidfd_open_callable, waitid_pidfd_callable) or (None, None).

  pidfd_open_callable(pid, flags=0) -> int, raises ProcessLookupError on ESRCH.
  waitid_pidfd_callable(fd, options) -> object with .si_pid/.si_status/.si_code
    (or None on WNOHANG no-event), raises ChildProcessError on ECHILD.
  """
  if hasattr(os, "pidfd_open") and hasattr(os, "P_PIDFD"):

    def _stdlib_pidfd_open(pid: int, flags: int = 0) -> int:
      return os.pidfd_open(pid, flags)

    def _stdlib_waitid_pidfd(fd: int, options: int):
      return os.waitid(os.P_PIDFD, fd, options)

    return _stdlib_pidfd_open, _stdlib_waitid_pidfd

  import ctypes
  import ctypes.util
  import platform

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

  def _ctypes_pidfd_open(pid: int, flags: int = 0) -> int:
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

    def __init__(self, si: _Siginfo):
      self.si_pid = si.si_pid
      self.si_uid = si.si_uid
      self.si_signo = si.si_signo
      self.si_status = si.si_status
      self.si_code = si.si_code

  P_PIDFD_CONST = 3

  def _ctypes_waitid_pidfd(fd: int, options: int):
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

# Probed once at import; stdlib-only and must not raise on slurm-less hosts.
_SACCT_AVAILABLE = shutil.which("sacct") is not None


# ---------------------------------------------------------------------------
# Probe subprocess plumbing (remote probes go over ssh; local sacct does not)
# ---------------------------------------------------------------------------
def _ssh_cmd(host: str, remote_cmd: str) -> list[str]:
  """Wrap a remote command in the batch-mode ssh invocation every remote probe uses."""
  return [
      "ssh",
      "-o",
      "BatchMode=yes",
      "-o",
      f"ConnectTimeout={_SSH_CONNECT_TIMEOUT}",
      host,
      remote_cmd,
  ]


async def _run_probe_cmd(
    cmd: list[str],
    *,
    timeout: float | None,
    kill_wait_log_event: str,
    **kill_wait_ctx: Any,
) -> tuple[bytes, bytes, int | None] | str:
  """Spawn ``cmd`` capturing stdout/stderr; return ``(stdout, stderr, returncode)``, or an error string.

  ``timeout=None`` waits without a deadline (local probes only); a timed-out
  probe is killed and awaited so no half-hung ssh is left behind.
  """
  try:
    proc = await asyncio.create_subprocess_exec(
        *cmd,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )
  except OSError as e:
    return f"spawn failed: {e}"

  if timeout is None:
    stdout_b, stderr_b = await proc.communicate()
  else:
    try:
      stdout_b, stderr_b = await asyncio.wait_for(proc.communicate(), timeout=timeout)
    except TimeoutError:
      proc.kill()
      try:
        await proc.wait()
      except Exception as e:
        log.debug(kill_wait_log_event, error=str(e), **kill_wait_ctx)
      return f"ssh timeout after {timeout}s"
  return stdout_b, stderr_b, proc.returncode


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
  run = await _run_probe_cmd(
      _ssh_cmd(host, f"kill -0 {pid} 2>&1 && echo ALIVE || echo DEAD"),
      timeout=_SSH_OVERALL_TIMEOUT,
      kill_wait_log_event="ssh_probe_wait_after_kill_failed",
      host=host,
      pid=pid,
  )
  if isinstance(run, str):
    return "ERROR", run
  stdout_b, stderr_b, returncode = run

  stdout = stdout_b.decode("utf-8", errors="replace")
  stderr = stderr_b.decode("utf-8", errors="replace")
  raw = stdout + stderr
  last_line = stdout.strip().splitlines()[-1].strip() if stdout.strip() else ""
  if last_line == "ALIVE":
    return "ALIVE", raw
  if last_line == "DEAD":
    return "DEAD", raw
  combined = (stdout + stderr).strip() or f"ssh exit {returncode}"
  return "ERROR", combined


async def _probe_remaining_remote_pids(
    remaining: dict[str, set[int]],
    trigger_id: str,
) -> list[str]:
  """Probe every (host, pid) in ``remaining`` concurrently via ssh.

  Mutates ``remaining`` in place: DEAD pids are discarded and hosts whose set
  goes empty are dropped. Returns ``host:pid`` labels for newly-exited pids;
  transient probe errors are logged and the pid stays under observation.
  """
  probes = [(host, pid) for host, pids in remaining.items() for pid in pids]
  results = await asyncio.gather(*[_ssh_probe_pid(h, p) for h, p in probes])
  newly_exited: list[str] = []
  for (host, pid), (status, raw) in zip(probes, results, strict=True):
    if status == "DEAD":
      remaining[host].discard(pid)
      if not remaining[host]:
        del remaining[host]
      newly_exited.append(f"{host}:{pid}")
    elif status == "ALIVE":
      continue
    else:
      # transient error / timeout — keep watching
      log.debug(
          "remote_probe_transient_error",
          trigger_id=trigger_id,
          host=host,
          pid=pid,
          raw=raw.strip(),
      )
  return newly_exited


# ---------------------------------------------------------------------------
# SLURM probe via sacct
# ---------------------------------------------------------------------------
async def _probe_sacct(
    job_ids: list[int],
    trigger_id: str,
    host: str | None = None,
) -> tuple[dict[int, tuple[str, str]], str | None]:
  """Run ``sacct`` once for the given job ids, locally or over ssh.

  Returns ``(states, error)``. ``states`` maps ``job_id -> (state, exit_code)`` for
  every row sacct reported; jobs absent from the output (slurmdbd accounting lag, job
  id not yet registered) are simply omitted — callers keep polling them rather than
  treating the gap as terminal. ``error`` is None on a successful probe, else a short
  description of why the probe itself failed (spawn error, ssh timeout, non-zero exit),
  with ``states`` empty.
  """
  ids = ",".join(str(j) for j in job_ids)
  sacct_args = ["sacct", "-j", ids, "-X", "-n", "-P", "--format=JobID,State,ExitCode"]
  # Remote probes get a deadline: ssh to a dead host would otherwise hang
  # forever. A local sacct goes bare; a wedged local slurmdbd is not a failure
  # mode this watcher bounds.
  if host is None:
    cmd = sacct_args
    timeout = None
  else:
    cmd = _ssh_cmd(host, " ".join(sacct_args))
    timeout = _SSH_OVERALL_TIMEOUT

  run = await _run_probe_cmd(
      cmd,
      timeout=timeout,
      kill_wait_log_event="sacct_probe_wait_after_kill_failed",
      trigger_id=trigger_id,
      host=host,
  )
  if isinstance(run, str):
    return {}, run
  stdout_b, stderr_b, returncode = run

  if returncode != 0:
    stderr = stderr_b.decode("utf-8", errors="replace").strip()
    log.warning(
        "sacct_probe_nonzero",
        trigger_id=trigger_id,
        host=host,
        returncode=returncode,
        stderr=stderr,
    )
    return {}, f"sacct exit {returncode}: {stderr}"

  states: dict[int, tuple[str, str]] = {}
  for line in stdout_b.decode("utf-8", errors="replace").splitlines():
    fields = line.strip().split("|")
    if len(fields) < 3:
      continue
    # Step rows (`123.batch`), array tasks (`123_4`) and heterogeneous components
    # (`123+0`) are not the allocation we asked about. `int()` accepts underscores as
    # digit separators, so `123_4` must be rejected by shape before conversion.
    if not fields[0].isdigit():
      continue
    states[int(fields[0])] = (fields[1].strip(), fields[2].strip())
  return states, None


def _slurm_label(host: str | None, job_id: int) -> str:
  """Watch label for a SLURM job: bare when local, host-prefixed when remote."""
  return f"slurm:{job_id}" if host is None else f"{host}:slurm:{job_id}"


# ---------------------------------------------------------------------------
# Schema migration: legacy `watch_pids: list[int]` -> `watch_targets: list[WatchTarget]`
# ---------------------------------------------------------------------------


def _migrate_legacy_watch_pids(raw_text: str) -> tuple[PendingTrigger, bool]:
  """Parse a trigger JSON string, upgrading legacy watch fields in-memory.

  Two legacy shapes are converged here (the single migration point):
    - the original `watch_pids: list[int]` -> `watch_targets` of local pids;
    - pre-discriminator `watch_targets` whose entries lack `kind` -> backfill
      LOCAL_PID when host is None, REMOTE_PID when a host is set.

  Returns (trigger, migrated). When `migrated` is True, the caller rewrites the
  file once in the new schema.
  """
  data = json.loads(raw_text)
  migrated = False
  if "watch_pids" in data:
    legacy = data.pop("watch_pids")
    if "watch_targets" not in data:
      data["watch_targets"] = [{"host": None, "pid": int(p)} for p in legacy] if legacy else []
    migrated = True
  for target in data.get("watch_targets") or []:
    if "kind" not in target:
      target["kind"] = (WatchKind.REMOTE_PID if target.get("host") is not None else WatchKind.LOCAL_PID).value
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
      probe_out: dict[str, str] | None = None,
  ) -> PendingTrigger:
    """Create a pending trigger, persist to disk, and start the sleep task.

    ``probe_out``, when given, is filled with ``label -> observed state`` for every
    remote SLURM target probed at create time, so the caller can report what was
    actually seen without probing twice.
    """
    targets = list(watch_targets or [])
    kinds = {t.kind for t in targets}

    if WatchKind.LOCAL_PID in kinds and not _PIDFD_SUPPORTED:
      raise RuntimeError("pidfd_open unavailable: need Linux 5.3+ with kernel pidfd_open support")
    if any(t.kind == WatchKind.SLURM_JOB and t.host is None for t in targets) and not _SACCT_AVAILABLE:
      raise RuntimeError("sacct unavailable: cannot watch a SLURM job on a host without slurm")

    if WatchKind.REMOTE_PID in kinds:
      await self._verify_remote_targets([t for t in targets if t.kind == WatchKind.REMOTE_PID])

    remote_slurm = [t for t in targets if t.kind == WatchKind.SLURM_JOB and t.host is not None]
    if remote_slurm:
      observed = await self._verify_remote_slurm_targets(remote_slurm)
      if probe_out is not None:
        probe_out.update(observed)

    fire_at = datetime.now(UTC) + timedelta(seconds=delay_seconds)
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

  async def _verify_remote_targets(self, targets: list[RemotePid]) -> None:
    """Probe each remote target once before persisting; reject if any not ALIVE."""
    results = await asyncio.gather(*[_ssh_probe_pid(t.host, t.pid) for t in targets])
    bad: list[str] = []
    for t, (status, raw) in zip(targets, results, strict=True):
      if status != "ALIVE":
        bad.append(f"{t.host}:{t.pid} -> {status} ({raw.strip()!r})")
    if bad:
      raise RemoteVerifyError("verify-on-create failed for remote watch target(s): " + "; ".join(bad))

  async def _verify_remote_slurm_targets(self, targets: list[SlurmJob]) -> dict[str, str]:
    """Probe each remote SLURM host once before persisting; reject if a probe failed.

    Returns ``label -> observed state``, using ``not-yet-registered`` when the probe
    worked but slurmdbd has no row for the job yet — accounting lag is normal right
    after ``sbatch`` and must not fail creation. Only a failed probe (ssh down, sacct
    missing, non-zero exit, timeout) is fatal, because that is the case where the
    trigger would silently degrade to a pure delay.
    """
    by_host: dict[str, list[int]] = {}
    for t in targets:
      by_host.setdefault(t.host, []).append(t.job_id)

    hosts = sorted(by_host)
    results = await asyncio.gather(*[
        _probe_sacct(sorted(by_host[h]), "verify-on-create", host=h) for h in hosts
    ])

    observed: dict[str, str] = {}
    bad: list[str] = []
    for host, (states, error) in zip(hosts, results, strict=True):
      if error is not None:
        bad.append(f"{host} -> {error}")
        continue
      for job_id in sorted(by_host[host]):
        row = states.get(job_id)
        observed[_slurm_label(host, job_id)] = row[0] if row is not None else "not-yet-registered"
    if bad:
      raise RemoteVerifyError("verify-on-create failed for remote SLURM host(s): " + "; ".join(bad))
    return observed

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

  async def _cancel_undeliverable(self, fresh: PendingTrigger, reason: str) -> None:
    """Every _wait_and_fire exit where the event cannot be delivered ends the same
    way: stamp CANCELLED, persist, drop the in-memory task handle, log the path's reason."""
    fresh.status = TriggerStatus.CANCELLED
    await self._save_trigger(fresh)
    self._tasks.pop(fresh.id, None)
    log.info(
        "trigger_cancelled_archived_session",
        trigger_id=fresh.id,
        session=fresh.session_id,
        reason=reason,
    )

  async def _wait_and_fire(self, trigger: PendingTrigger) -> None:
    """Wait until every watch group finishes (or fire_at), then trigger the master agent.

    Targets are grouped by kind and each group's sub-waiter runs concurrently
    against the shared ``fire_at`` deadline. The reason is 'completed' when all
    groups finish, 'timeout' if the deadline arrives with anything still alive.
    """
    groups: dict[WatchKind, list[WatchTarget]] = {}
    for t in trigger.watch_targets:
      groups.setdefault(t.kind, []).append(t)

    if groups:
      sub_waiters = []
      for kind, group in groups.items():
        if kind == WatchKind.LOCAL_PID:
          sub_waiters.append(self._wait_with_pidfd(trigger, group))
        elif kind == WatchKind.REMOTE_PID:
          sub_waiters.append(self._wait_with_remote_probe(trigger, group))
        elif kind == WatchKind.SLURM_JOB:
          sub_waiters.append(self._wait_with_sacct(trigger, group))
        elif kind == WatchKind.UNKNOWN:
          raise RuntimeError("WatchKind.UNKNOWN is a fail-loud sentinel and must never be watched")
        else:
          raise RuntimeError(f"unhandled WatchKind in dispatch: {kind!r}")
      results = await asyncio.gather(*sub_waiters)
      finished = [label for group_finished, _ in results for label in group_finished]
      still_alive = [label for _, group_alive in results for label in group_alive]
      reason = "timeout" if still_alive else "completed"
    else:
      now = datetime.now(UTC)
      remaining = (trigger.fire_at - now).total_seconds()
      if remaining > 0:
        await asyncio.sleep(remaining)
      reason = "timeout"
      finished = []
      still_alive = []

    # Re-load to check for cancellation during sleep
    try:
      fresh = await self._load_trigger(trigger.session_id, trigger.id)
    except FileNotFoundError:
      log.warning("trigger_file_missing_after_sleep", trigger_id=trigger.id)
      return
    if fresh.status != TriggerStatus.PENDING:
      return

    session_meta = await self._session_mgr.get_session(fresh.session_id)
    if session_meta is None:
      await self._cancel_undeliverable(fresh, reason="metadata_unavailable")
      return

    if fresh.watch_targets:
      suffix = _format_suffix(reason, finished, still_alive)
      trigger_message = f"[Scheduled trigger fired | {reason}] {fresh.message}{suffix}"
    else:
      trigger_message = f"[Scheduled trigger fired] {fresh.message}"

    # Resolve the succession chain end first so we can decide whether to cancel
    # or redirect delivery. An archived session with no successor is the user's
    # explicit "no more wakes" signal and cancels, as today.
    resolved_tail = await self._session_mgr.resolve_successor_chain(fresh.session_id)
    if resolved_tail is None:
      await self._cancel_undeliverable(fresh, reason="metadata_unavailable")
      return

    if resolved_tail.status == SessionStatus.ARCHIVED and resolved_tail.successor_session_id is None:
      await self._cancel_undeliverable(fresh, reason="archived")
      return

    # Deliver the scheduled-trigger event through the succession-aware primitive:
    # it persists into the chain end (stamping origin_session_id when redirected)
    # and returns None only when the chain end no longer exists.
    delivered = await self._session_mgr.deliver_to_successor(
        fresh.session_id,
        build_scheduled_trigger_event(trigger_message),
    )
    if delivered is None:
      await self._cancel_undeliverable(fresh, reason="chain_end_missing")
      return

    # Wake the master CC. Re-read the config here rather than using the snapshot
    # captured at construction: backends added to or renamed in config.yaml after
    # server start are invisible to that snapshot.
    await trigger_master(
        fresh.session_id,
        trigger_message,
        get_config(),
        self._session_mgr,
    )

    fresh.status = TriggerStatus.FIRED
    fresh.fired_at = datetime.now(UTC)
    fresh.fire_reason = reason
    await self._save_trigger(fresh)
    self._tasks.pop(trigger.id, None)
    log.info("trigger_fired", trigger_id=trigger.id, session=fresh.session_id, reason=reason)

  async def _wait_with_pidfd(
      self,
      trigger: PendingTrigger,
      targets: list[LocalPid],
  ) -> tuple[list[str], list[str]]:
    """Event-driven wait on local pidfds. Returns (finished, still_alive) labels.

    A pid that has already exited at start counts as finished (labelled "gone at
    start") and the wait continues for the rest — per-target, AND-correct. pidfd
    readiness cannot reap an arbitrary (non-child) pid, so finished labels carry
    no exit code.
    """
    loop = asyncio.get_running_loop()
    pidfds: dict[int, int] = {}  # fd -> pid
    finished: list[str] = []

    for t in targets:
      try:
        fd = _pidfd_open(t.pid)
      except ProcessLookupError:
        finished.append(f"{t.pid} (gone at start)")
        continue
      pidfds[fd] = t.pid

    if not pidfds:
      return finished, []

    done = asyncio.Event()

    def on_ready(fd: int) -> None:
      pid = pidfds.pop(fd, None)
      if pid is None:
        return
      with contextlib.suppress(Exception):
        loop.remove_reader(fd)
      # Reaps only when the watched pid is our child; waitid on a non-child
      # raises ChildProcessError and there is nothing to reap.
      with contextlib.suppress(ChildProcessError, OSError):
        _waitid_pidfd(fd, os.WEXITED | os.WNOHANG)
      with contextlib.suppress(OSError):
        os.close(fd)
      finished.append(str(pid))
      if not pidfds:
        done.set()

    for fd in list(pidfds):
      loop.add_reader(fd, on_ready, fd)

    now = datetime.now(UTC)
    remaining = (trigger.fire_at - now).total_seconds()
    try:
      await asyncio.wait_for(done.wait(), timeout=max(0.0, remaining))
    except TimeoutError:
      pass
    finally:
      # Cleanup any remaining fds (e.g. timeout case, or cancellation)
      for fd in list(pidfds):
        with contextlib.suppress(Exception):
          loop.remove_reader(fd)
        with contextlib.suppress(OSError):
          os.close(fd)

    still_alive = [str(p) for p in pidfds.values()]
    return finished, still_alive

  async def _wait_with_remote_probe(
      self,
      trigger: PendingTrigger,
      targets: list[RemotePid],
  ) -> tuple[list[str], list[str]]:
    """Polling wait via ssh probes with backoff. Returns (finished, still_alive).

    Labels use ``host:pid``. Verify-on-create has already confirmed every PID was
    ALIVE at create time.
    """
    remaining: dict[str, set[int]] = {}
    for t in targets:
      remaining.setdefault(t.host, set()).add(t.pid)

    finished: list[str] = []
    step = 0

    while True:
      now = datetime.now(UTC)
      time_to_fire = (trigger.fire_at - now).total_seconds()
      if time_to_fire <= 0:
        break

      base = _REMOTE_PROBE_INTERVALS[step] if step < len(_REMOTE_PROBE_INTERVALS) else _REMOTE_PROBE_PLATEAU
      sleep_for = base + random.uniform(0, _REMOTE_PROBE_NOISE_MAX)
      await asyncio.sleep(min(sleep_for, time_to_fire))

      now = datetime.now(UTC)
      if (trigger.fire_at - now).total_seconds() <= 0:
        break

      finished.extend(await _probe_remaining_remote_pids(remaining, trigger.id))

      step += 1
      if not remaining:
        break

    still_alive = [f"{host}:{pid}" for host, pids in remaining.items() for pid in pids]
    return finished, still_alive

  async def _wait_with_sacct(
      self,
      trigger: PendingTrigger,
      targets: list[SlurmJob],
  ) -> tuple[list[str], list[str]]:
    """Polling wait via ``sacct`` for SLURM jobs, local and remote.

    Targets are grouped by ``host`` (``None`` = the trigger-server host); each group
    runs its own probe loop concurrently, with one batched ``sacct`` per round.
    """
    groups: dict[str | None, set[int]] = {}
    for t in targets:
      groups.setdefault(t.host, set()).add(t.job_id)

    results = await asyncio.gather(*[
        self._wait_sacct_group(trigger, host, ids) for host, ids in groups.items()
    ])
    finished = [label for group_finished, _ in results for label in group_finished]
    still_alive = [label for _, group_alive in results for label in group_alive]
    return finished, still_alive

  async def _wait_sacct_group(
      self,
      trigger: PendingTrigger,
      host: str | None,
      job_ids: set[int],
  ) -> tuple[list[str], list[str]]:
    """Probe one host's SLURM jobs until all are terminal or ``fire_at`` arrives.

    Finished labels carry the authoritative State + ExitCode; still-alive labels are
    bare. The local group polls at a fixed interval; remote groups use the ssh backoff
    ladder because each probe costs an ssh round trip. On a host without sacct the
    local group skips polling and waits out the deadline rather than spinning on a
    missing binary; remote groups are unaffected. A remote group that has answered
    nothing for ``_REMOTE_SACCT_UNREACHABLE_GRACE`` stops waiting and reports its
    targets as unreachable so the trigger fires instead of going blind.
    """
    remaining = set(job_ids)

    if host is None and not _SACCT_AVAILABLE:
      log.warning("slurm_watch_no_sacct_skip", trigger_id=trigger.id, job_ids=sorted(remaining))
      time_to_fire = (trigger.fire_at - datetime.now(UTC)).total_seconds()
      if time_to_fire > 0:
        await asyncio.sleep(time_to_fire)
      return [], [_slurm_label(host, j) for j in sorted(remaining)]

    finished: list[str] = []
    step = 0
    last_success = datetime.now(UTC)
    while True:
      states, error = await _probe_sacct(sorted(remaining), trigger.id, host=host)
      if error is None:
        last_success = datetime.now(UTC)
      else:
        log.debug("sacct_probe_transient_error", trigger_id=trigger.id, host=host, error=error)
        dark_for = (datetime.now(UTC) - last_success).total_seconds()
        if host is not None and dark_for >= _REMOTE_SACCT_UNREACHABLE_GRACE:
          log.warning(
              "slurm_watch_host_unreachable",
              trigger_id=trigger.id,
              host=host,
              dark_seconds=int(dark_for),
              error=error,
          )
          note = error.splitlines()[0][:120]
          still_alive = [
              f"{_slurm_label(host, j)} (unreachable {int(dark_for // 60)}m: {note})"
              for j in sorted(remaining)
          ]
          return finished, still_alive
      for job_id in sorted(remaining):
        row = states.get(job_id)
        if row is None:
          continue  # accounting lag / not yet registered — keep polling
        state, exit_code = row
        state_key = state.split()[0] if state else ""
        if state_key in _SLURM_ACTIVE_STATES:
          continue
        if state_key not in _SLURM_TERMINAL_STATES:
          log.warning("slurm_unknown_state", trigger_id=trigger.id, job_id=job_id, state=state)
          continue
        finished.append(f"{_slurm_label(host, job_id)}: {state} {exit_code}")
        remaining.discard(job_id)

      if not remaining:
        break
      time_to_fire = (trigger.fire_at - datetime.now(UTC)).total_seconds()
      if time_to_fire <= 0:
        break
      if host is None:
        sleep_for = float(_SACCT_POLL_INTERVAL)
      else:
        base = _REMOTE_PROBE_INTERVALS[step] if step < len(_REMOTE_PROBE_INTERVALS) else _REMOTE_PROBE_PLATEAU
        sleep_for = base + random.uniform(0, _REMOTE_PROBE_NOISE_MAX)
      step += 1
      await asyncio.sleep(min(sleep_for, time_to_fire))

    still_alive = [_slurm_label(host, j) for j in sorted(remaining)]
    return finished, still_alive

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
    async with aiofiles.open(path) as f:
      raw = await f.read()
    trigger, _ = _migrate_legacy_watch_pids(raw)
    return trigger


def _format_suffix(
    reason: str,
    finished: list[str],
    still_alive: list[str],
) -> str:
  """Build the message suffix describing watch outcomes for a fired trigger.

  Labels are pre-formatted by each sub-waiter: `"1234"` (local pid, possibly
  "gone at start"), `"host:5678"` (remote pid), `"slurm:42: COMPLETED 0:0"`
  (slurm job).
  """
  finished_part = ", ".join(finished)
  if reason == "completed":
    return f" (finished: {finished_part})"
  # timeout
  alive_part = ", ".join(still_alive)
  if finished_part:
    return f" (finished: {finished_part}; still alive: {alive_part})"
  return f" (still alive: {alive_part})"
