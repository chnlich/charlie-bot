"""Process management utilities."""

import asyncio
import contextlib
import ctypes
import os
import signal
import sys
import time
from collections.abc import Callable, Coroutine
from dataclasses import dataclass
from pathlib import Path
from typing import Any, TypeVar

import structlog

from src.core.log_once import WarnOnceRegistry
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


# ---------------------------------------------------------------------------
# Session memory-cap cgroups (plan_01 v3)
#
# systemd delegates the user's app.slice subtree, so an unprivileged server can
# create one cgroup per session under it, hold every agent process the session
# spawns to hard memory.max / memory.swap.max limits, and let the kernel kill
# only the cgroup's largest process on a breach. Mechanism verified on this
# host 2026-09-11 (spike: mkdir + memory.max write + cgroup.procs move under
# asyncio.create_subprocess_exec with start_new_session=True).
# ---------------------------------------------------------------------------

# The user-delegated cgroup v2 subtree the session cgroups live under.
CGROUP_V2_APP_SLICE = "/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/app.slice"
# Directory-name prefix; the suffix is the session id's first 8 chars, so a
# human can match cgroup directories against the session list.
SESSION_CGROUP_PREFIX = "charliebot-sess-"

# Host-level degradation (missing delegation, read-only base) is a per-host
# state, not a per-spawn event: one line per failure shape per process.
_session_cgroup_degraded = WarnOnceRegistry()


def session_cgroup_name(session_id: str) -> str:
  """Cgroup directory name for *session_id*: ``charliebot-sess-<first 8 chars>``."""
  return f"{SESSION_CGROUP_PREFIX}{session_id[:8]}"


def session_cgroup_path(session_id: str) -> Path:
  """Full cgroup directory path for *session_id* under the app.slice base."""
  return Path(CGROUP_V2_APP_SLICE) / session_cgroup_name(session_id)


def ensure_session_cgroup(session_id: str, memory_max_mb: int, swap_max_mb: int) -> Path | None:
  """Create or refresh the session's memory-cap cgroup; None when the host cannot.

  Lazy per-session creation: the directory is made with the configured
  ``memory.max`` / ``memory.swap.max``; an existing directory (the session's
  later spawns) just has its limit files rewritten to the current config
  values. Any OSError degrades to None with one logged warning — cgroup
  control never blocks a spawn (plan_01 v3 §4.2 host guard).
  """
  path = session_cgroup_path(session_id)
  try:
    path.mkdir()
    log.info("session_cgroup_created", path=str(path), memory_max_mb=memory_max_mb, swap_max_mb=swap_max_mb)
  except FileExistsError:
    pass
  except OSError as e:
    _session_cgroup_degraded.log(log.warning, "session_cgroup_unavailable", str(e), path=str(path), error=str(e))
    return None
  try:
    (path / "memory.max").write_text(str(memory_max_mb * 1024 * 1024))
    (path / "memory.swap.max").write_text(str(swap_max_mb * 1024 * 1024))
  except OSError as e:
    _session_cgroup_degraded.log(log.warning, "session_cgroup_unavailable", str(e), path=str(path), error=str(e))
    return None
  return path


def read_memory_events(cgroup_dir: Path) -> tuple[int, int] | None:
  """``(max, oom_kill)`` counters from ``<cgroup_dir>/memory.events``, or None when unreadable.

  cgroup v2 kernel semantics (admin-guide/cgroup-v2): ``max`` counts the times
  the cgroup's own limit blocked an allocation (the cap-kill evidence), while
  ``oom_kill`` counts kills by any OOM including the host-wide one — the two
  readings the exit attribution (see :func:`classify_cgroup_exit`) tells apart.
  """
  try:
    text = (cgroup_dir / "memory.events").read_text(encoding="utf-8")
  except OSError:
    return None
  counts: dict[str, int] = {}
  for line in text.splitlines():
    key, _, raw = line.partition(" ")
    if key in ("max", "oom_kill"):
      try:
        counts[key] = int(raw.strip())
      except ValueError:
        return None
  if "max" not in counts or "oom_kill" not in counts:
    return None
  return counts["max"], counts["oom_kill"]


def make_session_cgroup_preexec(cgroup_dir: Path | None) -> Callable[[], None] | None:
  """preexec_fn moving the forked child into *cgroup_dir*, or None when cgroup control is off.

  Runs in the child between fork and exec: opens ``<cgroup>/cgroup.procs`` and
  writes ``0`` — the kernel convention that moves the calling process itself
  into the cgroup. A failure here raises and fails the spawn loudly: the
  directory was just created and verified writable by the parent, so an error
  is a system-level anomaly, not a degraded mode.
  """
  if cgroup_dir is None:
    return None
  procs_path = str(cgroup_dir / "cgroup.procs")

  def _preexec() -> None:
    fd = os.open(procs_path, os.O_WRONLY)
    try:
      os.write(fd, b"0")
    finally:
      os.close(fd)

  return _preexec


def compose_preexec(*preexecs: Callable[[], None] | None) -> Callable[[], None] | None:
  """One preexec_fn running every non-None *preexecs* in order; None when all are None.

  Spawn points that already carry a preexec (the pdeathsig pair) merge the
  cgroup move with it through this instead of replacing it, keeping the
  pdeathsig semantics intact.
  """
  fns = [fn for fn in preexecs if fn is not None]
  if not fns:
    return None
  if len(fns) == 1:
    return fns[0]

  def _preexec() -> None:
    for fn in fns:
      fn()

  return _preexec


@dataclass(frozen=True)
class SessionCgroup:
  """One spawn's handle on the session memory-cap cgroup (plan_01 v3).

  Carries the cgroup directory, the configured cap (for the report text), and
  the ``memory.events`` counters captured before the spawn — the baseline the
  exit attribution compares against.
  """

  path: Path
  memory_max_mb: int
  events_before: tuple[int, int] | None

  def classify_exit(self, returncode: int | None) -> str | None:
    """Attribution verdict for an exited process of this spawn (see :func:`classify_cgroup_exit`)."""
    return classify_cgroup_exit(returncode, self.events_before, read_memory_events(self.path), self.memory_max_mb)


def classify_cgroup_exit(
    returncode: int | None,
    before: tuple[int, int] | None,
    after: tuple[int, int] | None,
    memory_max_mb: int,
) -> str | None:
  """Cap vs host-OOM attribution for an exited process, or None when nothing is attributable.

  A SIGKILL exit whose ``memory.events`` ``max`` count grew over the spawn is a
  session-cap kill (the report routes heavy work to the remote cluster); a
  SIGKILL where only ``oom_kill`` grew means the host-wide OOM picked a cgroup
  member (reported as-is, no routing implication). Any other exit — clean exit,
  SIGTERM, or our own escalation kill with static counters — attributes nothing.
  """
  if returncode != -signal.SIGKILL:
    return None
  if before is None or after is None:
    return None
  if after[0] > before[0]:
    return (
        f"session 内存上限触发（上限 {memory_max_mb} MB），重任务请走集群三入口："
        "gpuq＝集群任务队列提交，ssh gate＝用 ssh 在远端跑把关测试，"
        "remote-launch＝charliebot 的远端启动命令")
  if after[1] > before[1]:
    return "进程被宿主机全局 OOM 终止（session 内存上限未触发）"
  return None


def prepare_session_cgroup(session_id: str | None, *, memory_max_mb: int, swap_max_mb: int) -> SessionCgroup | None:
  """Ensure the session's cgroup and snapshot its pre-spawn ``memory.events`` counters.

  Returns None when cgroup control is off for this spawn: *session_id* is None
  (a spawn with no session home), the configured cap is 0, or the host does not
  support user-owned cgroups — the spawn then proceeds exactly as before.
  """
  if not session_id or memory_max_mb <= 0:
    return None
  path = ensure_session_cgroup(session_id, memory_max_mb, swap_max_mb)
  if path is None:
    return None
  return SessionCgroup(path=path, memory_max_mb=memory_max_mb, events_before=read_memory_events(path))


def cleanup_session_cgroup(session_id: str) -> bool:
  """Remove the session's cgroup directory; True when removed.

  rmdir succeeds only for an empty cgroup — a directory still holding live
  processes stays, and the failure is logged at debug: the kernel reclaims the
  empty directory naturally once its last member exits.
  """
  path = session_cgroup_path(session_id)
  try:
    path.rmdir()
  except FileNotFoundError:
    return False
  except OSError as e:
    log.debug("session_cgroup_cleanup_retained", path=str(path), error=str(e))
    return False
  log.info("session_cgroup_removed", path=str(path))
  return True


def sweep_stale_session_cgroups() -> int:
  """Remove leftover ``charliebot-sess-*`` cgroups from a previous server life at startup.

  Only empty directories are removed; one still holding processes (a detached
  run outliving the restart) is kept with a warning — its members must not be
  re-homed by force. Returns the removed count.
  """
  base = Path(CGROUP_V2_APP_SLICE)
  try:
    entries = list(base.iterdir())
  except OSError as e:
    log.info("session_cgroup_sweep_unavailable", base=CGROUP_V2_APP_SLICE, error=str(e))
    return 0
  removed = 0
  for entry in entries:
    if not entry.name.startswith(SESSION_CGROUP_PREFIX) or not entry.is_dir():
      continue
    try:
      entry.rmdir()
      removed += 1
    except OSError as e:
      log.warning("session_cgroup_sweep_retained", path=str(entry), error=str(e))
  if removed:
    log.info("session_cgroup_sweep_removed", count=removed)
  return removed


def log_session_cgroup_startup(memory_max_mb: int, swap_max_mb: int, uncovered_backends: bool) -> None:
  """The one startup line stating whether session cgroup control is on (plan_01 v3 host guard).

  *uncovered_backends* is the caller's judgment that a configured backend
  spawns through the shared tmux server (claude-sub / tui-cli): those agent
  processes re-parent onto a pre-existing daemon, so the fork-time preexec
  cannot move them into the session's cgroup.
  """
  if memory_max_mb <= 0:
    log.info("session_cgroup_disabled", reason="server.session_memory_max_mb is 0")
    return
  base = Path(CGROUP_V2_APP_SLICE)
  if not base.is_dir():
    log.info("session_cgroup_disabled", reason=f"{CGROUP_V2_APP_SLICE} not present on this host")
    return
  if not os.access(base, os.W_OK):
    log.info("session_cgroup_disabled", reason=f"{CGROUP_V2_APP_SLICE} not writable by this user")
    return
  if uncovered_backends:
    log.warning(
        "session_cgroup_partial_coverage",
        detail="tmux-mediated backends (claude-sub / tui-cli) spawn under the shared tmux server "
        "and are not cgroup-covered")
  log.info("session_cgroup_enabled", memory_max_mb=memory_max_mb, swap_max_mb=swap_max_mb)
