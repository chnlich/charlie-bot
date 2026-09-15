"""The one home-level writer/migration exclusion fence.

A session-tree migration apply mutates a whole CharlieBot home while every
normal writer must be stopped. The boundary has to be enforced by a mechanism
the normal writers actually take — never by a caller-supplied boolean, a free
port, or a private lock the writers ignore:

- **The fence file** (``<home>/state/home_writer.lock``) carries an exclusive
  ``flock`` for the holder's whole lifetime. The server acquires it at startup
  and holds it until exit (``server.lifespan``); a migration apply acquires it
  for its whole run. The kernel releases the lock when the holder dies, so a
  crashed server never leaves a stale exclusion behind.
- **The identity record** (``<home>/state/writer_identity.json``) names the
  current holder (pid, /proc start time, argv, purpose, started_at) so a
  refused caller can report exactly who holds the home. It is evidence, not
  the exclusion: the flock is.
- :func:`probe_writer_fence` answers read-only questions (is a holder alive,
  which one) for dry-run reporting. A probe never signals a process.

Apply additionally scans ``/proc`` for live processes whose environment binds
them to this home (see :mod:`src.core.session_tree_migration`), which covers
writers predating this fence. A refused startup or apply exits with the
holder's details; nothing is ever killed by this module.
"""

import fcntl
import os
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path

from src.core.json_utils import atomic_write_text, load_json_meta
from src.core.log_once import LazyStructlogLogger
from src.core.runs import read_pid_stat

log = LazyStructlogLogger()

STATE_DIR_NAME = "state"
FENCE_LOCK_NAME = "home_writer.lock"
FENCE_IDENTITY_NAME = "writer_identity.json"


class HomeWriterActiveError(RuntimeError):
  """The home's writer fence is held by a live process (startup/apply refuses)."""

  def __init__(self, holder: "FenceHolder | None", home: Path, purpose: str) -> None:
    self.holder = holder
    self.home = home
    detail = (
        f"home writer fence held by pid {holder.pid} (started {holder.started_at}, "
        f"purpose {holder.purpose!r}, argv: {holder.argv})" if holder else
        "home writer fence is held (holder identity unreadable)")
    super().__init__(f"{purpose} refused for home {home}: {detail}")


@dataclass(frozen=True)
class FenceHolder:
  """The recorded identity of the fence's current holder."""
  pid: int
  pid_start: str | None
  started_at: str
  purpose: str
  argv: str
  home: str


def fence_lock_path(home: Path) -> Path:
  return home / STATE_DIR_NAME / FENCE_LOCK_NAME


def fence_identity_path(home: Path) -> Path:
  return home / STATE_DIR_NAME / FENCE_IDENTITY_NAME


def _pid_start_of(pid: int) -> str | None:
  pair = read_pid_stat(pid)
  return pair[0] if pair else None


def _holder_alive(holder: FenceHolder) -> bool:
  """Whether the recorded holder is still the same live process instance."""
  if holder.pid <= 0:
    return False
  pair = read_pid_stat(holder.pid)
  if pair is None or pair[1] == "Z":
    return False
  if holder.pid_start is not None and pair[0] != holder.pid_start:
    return False  # pid reused by another process: the recorded holder is gone
  return True


class HomeWriterFence:
  """An acquired exclusive home-writer exclusion; hold for the whole run."""

  def __init__(self, home: Path, purpose: str) -> None:
    self.home = home
    self.purpose = purpose
    self._fd: int | None = None

  def release(self) -> None:
    """Drop the exclusion and the identity record (idempotent)."""
    if self._fd is not None:
      try:
        fcntl.flock(self._fd, fcntl.LOCK_UN)
      finally:
        os.close(self._fd)
        self._fd = None
      path = fence_identity_path(self.home)
      if path.exists():
        path.unlink()

  def __enter__(self) -> "HomeWriterFence":
    return self

  def __exit__(self, *exc: object) -> None:
    self.release()


def acquire_home_writer_fence(home: Path, *, purpose: str) -> HomeWriterFence:
  """Take the home's exclusive writer exclusion, or refuse with holder details.

  The lock file is created if absent; the flock is exclusive and non-blocking,
  so a concurrent holder (another server, another apply) refuses this caller
  immediately instead of queueing. The identity record is written after the
  lock is held, so a crash between the two leaves the exclusion (kernel-held)
  without an identity row — a probe then reports an unidentifiable holder,
  never a falsely free home.
  """
  home = Path(home)
  lock_path = fence_lock_path(home)
  lock_path.parent.mkdir(parents=True, exist_ok=True)
  fd = os.open(lock_path, os.O_RDWR | os.O_CREAT, 0o600)
  try:
    fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
  except OSError as e:
    os.close(fd)
    raise HomeWriterActiveError(read_fence_holder(home), home, purpose) from e
  fence = HomeWriterFence(home, purpose)
  fence._fd = fd
  holder = FenceHolder(
      pid=os.getpid(),
      pid_start=_pid_start_of(os.getpid()),
      started_at=datetime.now(UTC).isoformat(),
      purpose=purpose,
      argv=" ".join(os.sys.argv),
      home=str(home),
  )
  atomic_write_text(fence_identity_path(home), _holder_json(holder))
  log.info("home_writer_fence_acquired", home=str(home), purpose=purpose, pid=holder.pid)
  return fence


def read_fence_holder(home: Path) -> FenceHolder | None:
  """The identity record's holder, or None when absent/unreadable."""
  raw = load_json_meta(fence_identity_path(home), "home_writer_identity_unreadable")
  if not isinstance(raw, dict):
    return None
  try:
    return FenceHolder(
        pid=int(raw["pid"]),
        pid_start=raw.get("pid_start"),
        started_at=str(raw.get("started_at", "")),
        purpose=str(raw.get("purpose", "")),
        argv=str(raw.get("argv", "")),
        home=str(raw.get("home", "")),
    )
  except (KeyError, TypeError, ValueError):
    return None


def _holder_json(holder: FenceHolder) -> str:
  import json
  return json.dumps({
      "pid": holder.pid,
      "pid_start": holder.pid_start,
      "started_at": holder.started_at,
      "purpose": holder.purpose,
      "argv": holder.argv,
      "home": holder.home,
  }, indent=2, sort_keys=True)


def probe_writer_fence(home: Path) -> dict:
  """Read-only fence status for dry-run reporting. Never signals a process.

  ``exclusive_holder_alive`` is proven by a non-blocking SHARED lock attempt:
  it succeeds only when no exclusive holder exists, and taking it momentarily
  mutates nothing. The identity row names the holder when it can.
  """
  home = Path(home)
  lock_path = fence_lock_path(home)
  status: dict = {
      "lock_path": str(lock_path),
      "identity_recorded": read_fence_holder(home),
      "exclusive_holder_alive": None,
  }
  if not lock_path.exists():
    status["exclusive_holder_alive"] = False
    return status
  fd = os.open(lock_path, os.O_RDWR)
  try:
    fcntl.flock(fd, fcntl.LOCK_SH | fcntl.LOCK_NB)
    fcntl.flock(fd, fcntl.LOCK_UN)
    status["exclusive_holder_alive"] = False
  except OSError:
    status["exclusive_holder_alive"] = True
  finally:
    os.close(fd)
  holder = status["identity_recorded"]
  if isinstance(holder, FenceHolder) and not _holder_alive(holder):
    # The lock is held by *someone* (the shared attempt failed) but the
    # recorded identity is not a live process: report both facts honestly.
    status["holder_identity_stale"] = True
  return status
