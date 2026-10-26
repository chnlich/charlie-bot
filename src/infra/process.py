"""Process management utilities."""

import asyncio
import dataclasses
import os
import pathlib
import signal
import time
from collections.abc import Callable, Coroutine
from typing import Any, TypeVar

from src.infra import log_once, tasks, timeouts

log = log_once.LazyStructlogLogger()

# Named TypeVar instead of PEP 695 ``wait_or_kill_group[T]``: yapf's pinned
# lib2to3 parser rejects PEP 695 type-parameter lists, and the inline form
# makes the whole tree unparseable to ``yapf -r``.
_T = TypeVar("_T")


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
  deadline = time.monotonic() + timeouts.KILL_ESCALATION_GRACE_SECONDS
  while is_alive() and time.monotonic() < deadline:
    await asyncio.sleep(timeouts.KILL_ESCALATION_POLL_SECONDS)
  if is_alive():
    kill_process_group(pid, signal.SIGKILL)


async def wait_or_kill_group(
    coro: Coroutine[Any, Any, _T], timeout: float | None, pid: int, stderr_task: asyncio.Task[bytes]) -> _T:
  """Await *coro* for at most *timeout* seconds (``None`` waits without a limit); cancel and drain *stderr_task*
  on every exit.

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
    await tasks.cancel_and_wait(stderr_task)


# ---------------------------------------------------------------------------
# Session cgroups: memory cap and task cap
#
# systemd delegates the user's subtree, so an unprivileged server can create
# one cgroup per session under the agents' leaf slice, hold every agent process the
# session spawns to hard memory.max / memory.swap.max limits, and let the
# kernel kill only the cgroup's largest process on a breach. pids.max caps the
# session's task count, so a runaway fork loop gets EAGAIN inside its own
# session and the kernel logs "cgroup: fork rejected by pids controller in
# <cgroup path>" instead of the host running out of PIDs. The parent creates
# the directory and writes the limits; the child-side preexec move runs only
# under asyncio.create_subprocess_exec with start_new_session=True — keep both
# the flag and the preexec hook on every session spawn path.
#
# The agents' leaf slice carries the host-level share, and its limit lives in
# the unit file under scripts/systemd/. Every session's cgroup sits inside it;
# the per-session caps below are unchanged.
# ---------------------------------------------------------------------------

# The user-delegated cgroup v2 subtree. systemd names each user's delegated
# subtree by uid (user-<uid>.slice/user@<uid>.service), so the path is built
# from this process's uid at import — a host whose user is not uid 1000 gets
# its own slice, not a hardcoded one.
CGROUP_V2_USER_SERVICE = f"/sys/fs/cgroup/user.slice/user-{os.getuid()}.slice/user@{os.getuid()}.service"


def user_slice_dir(unit: str) -> str:
  """The cgroup directory systemd gives user slice *unit*.

  systemd nests a slice inside the slice each dash prefix of its name names
  (``a-b.slice`` lives in ``a.slice/a-b.slice``), so the agents' leaf slice sits
  inside charliebot.slice, not beside it.
  """
  parts = unit.removesuffix(".slice").split("-")
  nested = "/".join("-".join(parts[:i]) + ".slice" for i in range(1, len(parts) + 1))
  return f"{CGROUP_V2_USER_SERVICE}/{nested}"


CGROUP_V2_AGENTS_SLICE = user_slice_dir("charliebot-agents.slice")
# The pre-split parent. Nothing is created here any more; the startup sweep
# still clears this home's empty leftovers, and a process already running in
# it stays until it exits.
CGROUP_V2_APP_SLICE = f"{CGROUP_V2_USER_SERVICE}/app.slice"
# Directory-name prefix; the suffix is the session id's first 8 chars, so a
# human can match cgroup directories against the session list.
SESSION_CGROUP_PREFIX = "charliebot-sess-"

# Every agent process carries this OOM priority, so a host-wide OOM picks an
# agent process before the server, which keeps the default 0. The value is
# positive, which an unprivileged writer may set.
SESSION_CGROUP_OOM_SCORE_ADJ = 500


class SessionCgroupUnavailableError(RuntimeError):
  """The session's slice is missing or not writable, so the spawn has no memory guard.

  The launcher's preflight and setup.sh install and start the slice; a host
  that lost it must stop the spawn visibly instead of running it unbounded.
  """


def session_cgroup_slices() -> tuple[str, ...]:
  """Every directory a session cgroup may live under, the retired app.slice included."""
  return (CGROUP_V2_AGENTS_SLICE, CGROUP_V2_APP_SLICE)


def session_cgroup_name(session_id: str) -> str:
  """Cgroup directory name for *session_id*: ``charliebot-sess-<first 8 chars>``."""
  return f"{SESSION_CGROUP_PREFIX}{session_id[:8]}"


def ensure_session_cgroup(session_id: str, memory_max_mb: int, swap_max_mb: int, tasks_max: int) -> pathlib.Path:
  """Create or refresh the session's cgroup under the agents' slice; raise when the slice cannot hold it.

  Lazy per-session creation: the directory is made with the configured
  ``memory.max`` / ``memory.swap.max`` / ``pids.max``; an existing directory
  (the session's later spawns) just has its limit files rewritten to the
  current config values. *tasks_max* 0 writes ``max`` (no task cap). A missing
  or unwritable slice, and any other OSError, raises instead of degrading: an
  unbounded spawn is the failure this guard exists to prevent.
  """
  base = pathlib.Path(CGROUP_V2_AGENTS_SLICE)
  if not base.is_dir() or not os.access(base, os.W_OK):
    raise SessionCgroupUnavailableError(
        f"session cgroup slice {base.name} ({base}) is missing or not writable; "
        "install and start the slice from scripts/systemd/ with scripts/setup.sh")
  # systemd enables a slice's controllers for the units inside it only, and a
  # session cgroup is no unit: without this its limit files would not exist.
  try:
    (base / "cgroup.subtree_control").write_text("+memory +pids")
  except OSError as e:
    raise SessionCgroupUnavailableError(
        f"session cgroup slice {base.name} cannot enable memory and pids for its children: {e}") from e
  path = base / session_cgroup_name(session_id)
  try:
    path.mkdir()
    log.info(
        "session_cgroup_created",
        path=str(path),
        memory_max_mb=memory_max_mb,
        swap_max_mb=swap_max_mb,
        tasks_max=tasks_max)
  except FileExistsError:
    pass
  except OSError as e:
    raise SessionCgroupUnavailableError(f"session cgroup slice {base.name} cannot hold {path.name}: {e}") from e
  try:
    (path / "memory.max").write_text(str(memory_max_mb * 1024 * 1024))
    (path / "memory.swap.max").write_text(str(swap_max_mb * 1024 * 1024))
    (path / "pids.max").write_text(str(tasks_max) if tasks_max > 0 else "max")
  except OSError as e:
    raise SessionCgroupUnavailableError(
        f"session cgroup slice {base.name} cannot take the limit files in {path}: {e}") from e
  return path


def read_memory_events(cgroup_dir: pathlib.Path) -> tuple[int, int] | None:
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


@dataclasses.dataclass(frozen=True)
class SessionCgroup:
  """One spawn's handle on the session cgroup.

  Carries the cgroup directory, the OOM priority of every agent process, the
  configured cap (for the report text), and the ``memory.events`` counters
  captured before the spawn — the baseline the exit attribution compares
  against.
  """

  path: pathlib.Path
  oom_score_adj: int
  memory_max_mb: int
  events_before: tuple[int, int] | None

  def classify_exit(self, returncode: int | None) -> str | None:
    """Attribution verdict for an exited process of this spawn (see :func:`classify_cgroup_exit`)."""
    return classify_cgroup_exit(returncode, self.events_before, read_memory_events(self.path), self.memory_max_mb)


def make_session_cgroup_preexec(session_cgroup: SessionCgroup | None) -> Callable[[], None] | None:
  """preexec_fn moving the forked child into its session cgroup and setting its OOM priority.

  Runs in the child between fork and exec: opens ``<cgroup>/cgroup.procs`` and
  writes ``0`` — the kernel convention that moves the calling process itself
  into the cgroup — then writes the session's positive ``oom_score_adj`` to
  ``/proc/self/oom_score_adj``. A failure here raises and fails the spawn
  loudly: the directory was just created and verified writable by the parent,
  so an error is a system-level anomaly, not a degraded mode. None means
  cgroup control is off for this spawn, and then no OOM priority is set either.
  """
  if session_cgroup is None:
    return None
  procs_path = str(session_cgroup.path / "cgroup.procs")
  oom_score_adj = str(session_cgroup.oom_score_adj).encode()

  def _preexec() -> None:
    fd = os.open(procs_path, os.O_WRONLY)
    try:
      os.write(fd, b"0")
    finally:
      os.close(fd)
    fd = os.open("/proc/self/oom_score_adj", os.O_WRONLY)
    try:
      os.write(fd, oom_score_adj)
    finally:
      os.close(fd)

  return _preexec


def make_nice_preexec(increment: int) -> Callable[[], None]:
  """preexec_fn raising the forked child's nice value by *increment*.

  Runs in the child between fork and exec, where ``os.nice`` is one syscall
  with no locks. A positive increment needs no privilege, and the child's
  whole process tree (the agent CLI and the tool subprocesses it spawns)
  inherits the value.
  """

  def _preexec() -> None:
    os.nice(increment)

  return _preexec


def compose_preexec(*preexecs: Callable[[], None] | None) -> Callable[[], None] | None:
  """One preexec_fn running every non-None *preexecs* in order; None when all are None.

  Spawn points that already carry a preexec merge the cgroup move with it
  through this instead of replacing it.
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


def classify_cgroup_exit(
    returncode: int | None,
    before: tuple[int, int] | None,
    after: tuple[int, int] | None,
    memory_max_mb: int,
) -> str | None:
  """Cap vs host-OOM attribution for an exited process, or None when nothing is attributable.

  A SIGKILL exit whose ``memory.events`` ``max`` count grew over the spawn is a
  session-cap kill (the report routes heavy work to the remote cluster); a
  SIGKILL where only ``oom_kill`` grew means the agents' share or the whole host
  ran out of memory (reported as-is, no routing implication). Any other exit —
  clean exit, SIGTERM, or our own escalation kill with static counters —
  attributes nothing.
  """
  if returncode != -signal.SIGKILL:
    return None
  if before is None or after is None:
    return None
  if after[0] > before[0]:
    return (
        f"session 内存上限触发（上限 {memory_max_mb} MB），重任务请走集群三入口："
        "gpuq＝集群任务队列提交，ssh gate＝用 ssh 在远端跑把关测试，"
        "远端启动＝charliebot 在远端启动长任务的命令")
  if after[1] > before[1]:
    return f"进程因内存不足被内核结束（agent 组或整机内存用满；session 内存上限 {memory_max_mb} MB 未触发）"
  return None


def prepare_session_cgroup(
    session_id: str | None,
    *,
    memory_max_mb: int,
    swap_max_mb: int,
    tasks_max: int,
) -> SessionCgroup | None:
  """Ensure the session's cgroup under the agents' slice and snapshot its ``memory.events``.

  Returns None only when cgroup control is off for this spawn: *session_id* is
  None (a spawn with no session home) or the configured memory cap is 0 — the
  spawn then proceeds exactly as before. With control on, a slice that cannot
  hold the cgroup raises :class:`SessionCgroupUnavailableError`.
  """
  if not session_id or memory_max_mb <= 0:
    return None
  path = ensure_session_cgroup(session_id, memory_max_mb, swap_max_mb, tasks_max)
  return SessionCgroup(
      path=path,
      oom_score_adj=SESSION_CGROUP_OOM_SCORE_ADJ,
      memory_max_mb=memory_max_mb,
      events_before=read_memory_events(path))


def cleanup_session_cgroup(session_id: str) -> bool:
  """Remove the session's cgroup directory from every slice; True when one was removed.

  A session's directory lands under the slice its last spawn chose, and that
  choice may have changed since; the retired app.slice is included so a
  directory left by the pre-split server is cleared too. rmdir succeeds only
  for an empty cgroup — a directory still holding live processes stays, and
  the failure is logged at debug: the kernel reclaims the empty directory
  naturally once its last member exits.
  """
  removed = False
  name = session_cgroup_name(session_id)
  for base in session_cgroup_slices():
    path = pathlib.Path(base) / name
    try:
      path.rmdir()
    except FileNotFoundError:
      continue
    except OSError as e:
      log.debug("session_cgroup_cleanup_retained", path=str(path), error=str(e))
      continue
    log.info("session_cgroup_removed", path=str(path))
    removed = True
  return removed


def sweep_stale_session_cgroups(sessions_dir: pathlib.Path) -> int:
  """Remove this home's leftover session cgroups from a previous server life at startup.

  A cgroup belongs to this home when its name is ``session_cgroup_name`` of a
  session directory under *sessions_dir*. The walk covers both leaf slices and
  the retired app.slice: processes that already run in app.slice stay there
  until they exit, and their empty directory is reclaimed here. Any other
  entry stays: other homes' cgroups and unrelated scopes alike. Only empty
  directories are removed; one still holding processes (a detached run
  outliving the restart) is kept with a warning — its members must not be
  re-homed by force. Returns the removed count.
  """
  owned = {session_cgroup_name(session.name) for session in sessions_dir.iterdir() if session.is_dir()}
  removed: list[str] = []
  for base in session_cgroup_slices():
    try:
      entries = list(pathlib.Path(base).iterdir())
    except OSError as e:
      log.info("session_cgroup_sweep_unavailable", base=base, error=str(e))
      continue
    for entry in entries:
      if entry.name not in owned or not entry.is_dir():
        continue
      try:
        entry.rmdir()
        removed.append(entry.name)
      except OSError as e:
        log.warning("session_cgroup_sweep_retained", path=str(entry), error=str(e))
  if removed:
    log.info("session_cgroup_sweep_removed", count=len(removed), names=sorted(removed))
  return len(removed)


def log_session_cgroup_startup(memory_max_mb: int, swap_max_mb: int, tasks_max: int) -> None:
  """The one startup line stating whether session cgroup control is on (the host guard)."""
  if memory_max_mb <= 0:
    log.info("session_cgroup_disabled", reason="server.session_memory_max_mb is 0")
    return
  base = CGROUP_V2_AGENTS_SLICE
  if not pathlib.Path(base).is_dir():
    log.warning("session_cgroup_unavailable", reason=f"{base} not present on this host; spawns with a session fail")
    return
  if not os.access(base, os.W_OK):
    log.warning("session_cgroup_unavailable", reason=f"{base} not writable by this user; spawns with a session fail")
    return
  log.info("session_cgroup_enabled", memory_max_mb=memory_max_mb, swap_max_mb=swap_max_mb, tasks_max=tasks_max)
