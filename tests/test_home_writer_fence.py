"""The home writer fence: the shared writer/migration exclusion.

The exclusion must be a mechanism normal writers actually take (the server
holds the same flock for its whole lifetime), never a caller-supplied boolean
or a private lock. These tests cover acquisition, mutual exclusion with
holder identity, the read-only probe, pid-reuse staleness, and the server
lifespan's refuse-to-start / release-on-exit behavior.
"""

from __future__ import annotations

import os
import subprocess
import sys
import time
from pathlib import Path

import pytest

from src.core.home_writer_fence import (
    HomeWriterActiveError,
    acquire_home_writer_fence,
    fence_identity_path,
    fence_lock_path,
    probe_writer_fence,
    read_fence_holder,
)


def _write_holder_identity(home: Path, pid: int, pid_start: str | None) -> None:
  import json

  from src.core.json_utils import atomic_write_text
  (home / "state").mkdir(parents=True, exist_ok=True)
  atomic_write_text(fence_identity_path(home), json.dumps({
      "pid": pid, "pid_start": pid_start, "started_at": "2026-01-01T00:00:00+00:00",
      "purpose": "test", "argv": "pytest", "home": str(home),
  }))


def test_acquire_excludes_and_reports_holder(tmp_path: Path) -> None:
  home = tmp_path / "home"
  home.mkdir()
  first = acquire_home_writer_fence(home, purpose="first")
  try:
    assert read_fence_holder(home) is not None
    with pytest.raises(HomeWriterActiveError) as excinfo:
      acquire_home_writer_fence(home, purpose="second")
    holder = excinfo.value.holder
    assert holder is not None and holder.pid == os.getpid()
    assert holder.purpose == "first"
    assert "first" in str(excinfo.value)
  finally:
    first.release()
  # Release drops the identity row and frees the lock.
  assert not fence_identity_path(home).exists()
  second = acquire_home_writer_fence(home, purpose="second")
  second.release()


def test_probe_reports_holder_without_taking_the_exclusion(tmp_path: Path) -> None:
  home = tmp_path / "home"
  home.mkdir()
  assert probe_writer_fence(home)["exclusive_holder_alive"] is False
  fence = acquire_home_writer_fence(home, purpose="probe-target")
  try:
    status = probe_writer_fence(home)
    assert status["exclusive_holder_alive"] is True
    holder = status["identity_recorded"]
    assert holder is not None and holder.pid == os.getpid()
  finally:
    fence.release()
  assert probe_writer_fence(home)["exclusive_holder_alive"] is False


def test_stale_identity_record_does_not_fake_a_holder(tmp_path: Path) -> None:
  home = tmp_path / "home"
  home.mkdir()
  _write_holder_identity(home, 999999, "424242")  # a dead, reused-style record
  status = probe_writer_fence(home)
  assert status["exclusive_holder_alive"] is False
  # A fresh acquire still works and overwrites the stale row.
  fence = acquire_home_writer_fence(home, purpose="after-stale")
  try:
    assert read_fence_holder(home).pid == os.getpid()
  finally:
    fence.release()


def test_external_process_holding_fence_is_visible(tmp_path: Path) -> None:
  """A real second process holding the fence blocks acquisition with its identity."""
  home = tmp_path / "home"
  home.mkdir()
  holder_code = (
      "import sys, time\n"
      f"sys.path.insert(0, {str(Path(__file__).parent.parent)!r})\n"
      "from src.core.home_writer_fence import acquire_home_writer_fence\n"
      f"fence = acquire_home_writer_fence({str(home)!r}, purpose='external')\n"
      "time.sleep(60)\n"
  )
  proc = subprocess.Popen([sys.executable, "-c", holder_code],
                          stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
  try:
    # Wait until the child provably holds the exclusion (real mechanism, not a
    # caller-supplied boolean), bounded so a broken child fails the test fast.
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
      if probe_writer_fence(home)["exclusive_holder_alive"]:
        break
      if proc.poll() is not None:
        pytest.fail(f"holder process exited early with rc={proc.returncode}")
      time.sleep(0.1)
    else:
      pytest.fail("holder process never acquired the fence within 15s")
    status = probe_writer_fence(home)
    holder = status["identity_recorded"]
    assert holder is not None and holder.pid == proc.pid
    with pytest.raises(HomeWriterActiveError) as excinfo:
      acquire_home_writer_fence(home, purpose="cli")
    assert excinfo.value.holder is not None
    assert excinfo.value.holder.pid == proc.pid
    assert excinfo.value.holder.purpose == "external"
  finally:
    proc.kill()
    proc.wait()
  # The kernel released the flock when the holder died, even though its
  # identity row (written under the lock) still sits on disk.
  assert fence_identity_path(home).exists()
  fence = acquire_home_writer_fence(home, purpose="after-crash")
  fence.release()


# --- server lifespan integration --------------------------------------------


class _AsyncStub:
  """A callable returning a no-op coroutine (optionally with a result)."""

  def __init__(self, return_value=None):
    self._return_value = return_value

  def __call__(self, *args, **kwargs):
    async def _noop(*a, **k):
      return self._return_value
    return _noop()


class _StubScheduler:
  def __init__(self, *a, **k):
    pass

  async def start(self):
    return None

  async def stop(self):
    return None


class _StubTriggerManager:
  def __init__(self, *a, **k):
    pass

  async def recover_pending(self):
    return None


@pytest.fixture
def lifespan_env(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
  """A minimal server-lifespan environment over one synthetic home.

  Only the doors that would touch the filesystem or the network are stubbed;
  the lifespan itself (and therefore its fence acquisition) is the real one.
  """
  import server as server_module
  from src.core.config import CharlieBotConfig

  home = tmp_path / "home"
  (home / "sessions").mkdir(parents=True)
  cfg = CharlieBotConfig(charliebot_home=home)
  monkeypatch.setenv("CHARLIEBOT_HOME", str(home))
  from conftest import reset_config_caches
  reset_config_caches()

  monkeypatch.setattr(server_module, "get_config", lambda: cfg)
  monkeypatch.setattr(server_module, "reconcile_master_identity",
                      _AsyncStub(return_value=None))
  monkeypatch.setattr(server_module, "_run_crash_recovery", _AsyncStub())
  monkeypatch.setattr(server_module, "_provision_speech_models", lambda cfg: None)
  monkeypatch.setattr(server_module, "log_session_cgroup_startup", lambda *a, **k: None)
  monkeypatch.setattr(server_module, "sweep_stale_session_cgroups", lambda *a, **k: None)
  monkeypatch.setattr(server_module, "Scheduler", _StubScheduler)
  monkeypatch.setattr(server_module, "TriggerManager", _StubTriggerManager)
  monkeypatch.setattr(server_module, "set_trigger_manager", lambda *a, **k: None)
  monkeypatch.setattr(server_module, "close_http_client", _AsyncStub())
  monkeypatch.setattr(server_module.streaming_manager, "close_all", _AsyncStub())
  monkeypatch.setattr(server_module.ext_usage, "start_poller", _AsyncStub())
  monkeypatch.setattr(server_module.ext_usage, "stop_poller", _AsyncStub())
  from src.core import task_recovery
  monkeypatch.setattr(task_recovery, "reconcile_task_tree", _AsyncStub(return_value={}))
  return server_module


@pytest.mark.asyncio
async def test_server_lifespan_acquires_and_releases_fence(lifespan_env) -> None:
  from fastapi import FastAPI
  server_module = lifespan_env
  home = server_module.get_config().charliebot_home
  async with server_module.lifespan(FastAPI()):
    status = probe_writer_fence(home)
    assert status["exclusive_holder_alive"] is True
    holder = status["identity_recorded"]
    assert holder is not None and holder.pid == os.getpid()
    assert holder.purpose == "server startup"
    # A concurrent apply (or a second server) refuses while the server runs.
    with pytest.raises(HomeWriterActiveError):
      acquire_home_writer_fence(home, purpose="session-tree migrate --apply")
  assert probe_writer_fence(home)["exclusive_holder_alive"] is False


@pytest.mark.asyncio
async def test_server_startup_refuses_while_apply_holds_fence(lifespan_env) -> None:
  from fastapi import FastAPI
  server_module = lifespan_env
  home = server_module.get_config().charliebot_home
  apply_fence = acquire_home_writer_fence(home, purpose="session-tree migrate --apply")
  try:
    with pytest.raises(HomeWriterActiveError) as excinfo:
      async with server_module.lifespan(FastAPI()):
        pass
    assert excinfo.value.holder is not None
    assert "apply" in excinfo.value.holder.purpose
  finally:
    apply_fence.release()
  # After the apply releases, startup works.
  async with server_module.lifespan(FastAPI()):
    pass


# ---------------------------------------------------------------------------
# Fence failure paths: ownership survives every failure mode
# ---------------------------------------------------------------------------


def test_failed_identity_publication_releases_the_lock_and_fd(tmp_path: Path) -> None:
  """A fence whose identity record cannot be written releases the exclusion and
  its fd: a half-acquired fence is never left behind in a live process."""
  import src.core.home_writer_fence as fence_module
  home = tmp_path / "home"
  home.mkdir()
  fds_before = set(os.listdir("/proc/self/fd"))
  original_write = fence_module.atomic_write_text

  def failing_write(*args, **kwargs):
    raise OSError("simulated identity-publication failure")

  fence_module.atomic_write_text = failing_write
  try:
    with pytest.raises(OSError, match="identity-publication failure"):
      acquire_home_writer_fence(home, purpose="failing")
  finally:
    fence_module.atomic_write_text = original_write
  # No fd leaked from the failed acquisition.
  assert set(os.listdir("/proc/self/fd")) == fds_before
  # The exclusion is gone: a fresh acquire takes it immediately.
  fence = acquire_home_writer_fence(home, purpose="after-failure")
  try:
    assert probe_writer_fence(home)["exclusive_holder_alive"] is True
  finally:
    fence.release()
  assert not fence_identity_path(home).exists()


def test_release_does_not_erase_a_successor_identity(tmp_path: Path) -> None:
  """The identity record is unlinked while the lock is still held, so a
  successor that acquires the exclusion the instant it is let go keeps its
  identity; the lock, not the record, is the exclusion."""
  import fcntl
  home = tmp_path / "home"
  home.mkdir()
  fence = acquire_home_writer_fence(home, purpose="first")
  lock_path = fence_lock_path(home)
  identity_path = fence_identity_path(home)
  successor = {"pid": 999999, "pid_start": "424242",
               "started_at": "2026-01-01T00:00:00+00:00", "purpose": "second",
               "argv": "successor", "home": str(home)}
  real_flock = fcntl.flock

  def flock_with_successor(fd, cmd):
    result = real_flock(fd, cmd)
    if cmd & fcntl.LOCK_UN:
      # A successor grabs the exclusion the moment it is let go and writes its
      # own identity before the previous holder's release returns.
      fd2 = os.open(lock_path, os.O_RDWR)
      try:
        real_flock(fd2, fcntl.LOCK_EX | fcntl.LOCK_NB)
        import json

        from src.core.json_utils import atomic_write_text
        atomic_write_text(identity_path, json.dumps(successor))
      finally:
        real_flock(fd2, fcntl.LOCK_UN)
        os.close(fd2)
    return result

  original_flock = fcntl.flock
  fcntl.flock = flock_with_successor
  try:
    fence.release()
  finally:
    fcntl.flock = original_flock
  holder = read_fence_holder(home)
  assert holder is not None and holder.pid == 999999
  # The lock itself is free (the simulated successor released it).
  assert probe_writer_fence(home)["exclusive_holder_alive"] is False


def test_fence_refuses_symlinked_paths(tmp_path: Path) -> None:
  """A symlinked state directory, lock, or identity record refuses acquisition
  and probing instead of placing or reading the exclusion outside the home."""
  from src.core.home_writer_fence import FencePathRefusalError
  outside = tmp_path / "outside"
  outside.mkdir()
  # (a) state itself is a symlink.
  home = tmp_path / "home_a"
  home.mkdir()
  (home / "state").symlink_to(outside)
  with pytest.raises(FencePathRefusalError, match="symlink"):
    acquire_home_writer_fence(home, purpose="cli")
  with pytest.raises(FencePathRefusalError, match="symlink"):
    probe_writer_fence(home)
  assert not (outside / "home_writer.lock").exists()
  # (b) the lock file is a symlink inside a real state dir.
  home = tmp_path / "home_b"
  (home / "state").mkdir(parents=True)
  (home / "state" / "home_writer.lock").symlink_to(outside / "captured.lock")
  with pytest.raises(FencePathRefusalError, match="symlink"):
    acquire_home_writer_fence(home, purpose="cli")
  with pytest.raises(FencePathRefusalError, match="symlink"):
    probe_writer_fence(home)
  assert not (outside / "captured.lock").exists()
  # (c) a real fence still works after the refusals.
  fence = acquire_home_writer_fence(tmp_path / "home_c", purpose="real")
  fence.release()


@pytest.mark.asyncio
async def test_server_lifespan_releases_fence_on_startup_failure(
    lifespan_env, monkeypatch) -> None:
  """A startup exception after acquisition releases the exclusion in the
  still-live process."""
  from fastapi import FastAPI
  server_module = lifespan_env
  home = server_module.get_config().charliebot_home

  def failing_build_info():
    raise RuntimeError("startup failed after the fence")

  monkeypatch.setattr(server_module, "init_build_info", failing_build_info)
  with pytest.raises(RuntimeError, match="startup failed after the fence"):
    async with server_module.lifespan(FastAPI()):
      pass
  status = probe_writer_fence(home)
  assert status["exclusive_holder_alive"] is False
  # A later server (or apply) can take the exclusion.
  fence = acquire_home_writer_fence(home, purpose="after-failed-startup")
  fence.release()


@pytest.mark.asyncio
async def test_server_lifespan_releases_fence_on_shutdown_failure(
    lifespan_env, monkeypatch) -> None:
  """A shutdown exception releases the exclusion; the fence never outlives the
  server's ability to hold it cleanly."""
  from fastapi import FastAPI
  server_module = lifespan_env
  home = server_module.get_config().charliebot_home

  class _FailingScheduler(_StubScheduler):
    async def stop(self):
      raise RuntimeError("shutdown failed")

  monkeypatch.setattr(server_module, "Scheduler", _FailingScheduler)
  with pytest.raises(RuntimeError, match="shutdown failed"):
    async with server_module.lifespan(FastAPI()):
      assert probe_writer_fence(home)["exclusive_holder_alive"] is True
  assert probe_writer_fence(home)["exclusive_holder_alive"] is False
  fence = acquire_home_writer_fence(home, purpose="after-failed-shutdown")
  fence.release()
