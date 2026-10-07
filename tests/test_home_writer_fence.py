"""The home writer fence: the shared writer/migration exclusion.

The exclusion must be a mechanism normal writers actually take (the server
holds the same flock for its whole lifetime), never a caller-supplied boolean
or a private lock. These tests cover acquisition, mutual exclusion with
holder identity, the read-only probe, pid-reuse staleness, and the server
lifespan's refuse-to-start / release-on-exit behavior.
"""

from __future__ import annotations

import os
from pathlib import Path

import pytest

from src.runtime.home_writer_fence import (
    HomeWriterActiveError,
    acquire_home_writer_fence,
    fence_identity_path,
    probe_writer_fence,
    read_fence_holder,
)


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
  from src.infra.config import CharlieBotConfig

  home = tmp_path / "home"
  (home / "sessions").mkdir(parents=True)
  cfg = CharlieBotConfig(charliebot_home=home)
  monkeypatch.setenv("CHARLIEBOT_HOME", str(home))
  from conftest import reset_config_caches
  reset_config_caches()

  monkeypatch.setattr(server_module, "get_config", lambda: cfg)
  monkeypatch.setattr(server_module.init_master_recovery, "reconcile_master_identity", _AsyncStub(return_value=None))
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
  from src.runtime import task_recovery
  monkeypatch.setattr(task_recovery, "reconcile_task_tree", _AsyncStub(return_value={}))
  return server_module


@pytest.mark.asyncio
async def test_server_startup_preloads_the_usage_tally_stack(lifespan_env, monkeypatch) -> None:
  """Startup runs the tally-stack preload as its own background task, so the page's and the
  ledger handler's request-time first-imports resolve against pinned modules instead of
  whatever files a mid-flight deploy left under the started server."""
  import asyncio

  from fastapi import FastAPI

  import src.app.pages as pages_module
  server_module = lifespan_env
  calls: list[int] = []
  monkeypatch.setattr(pages_module, "preload_usage_tally_stack", lambda: calls.append(1))
  app = FastAPI()
  async with server_module.lifespan(app):
    await asyncio.wait_for(app.state.usage_tally_warmup_task, timeout=10)
  assert calls == [1]


@pytest.mark.asyncio
async def test_server_startup_refuses_while_apply_holds_fence(lifespan_env) -> None:
  from fastapi import FastAPI
  server_module = lifespan_env
  home = server_module.get_config().charliebot_home
  apply_fence = acquire_home_writer_fence(home, purpose="session-tree preview --add-backend")
  try:
    with pytest.raises(HomeWriterActiveError) as excinfo:
      async with server_module.lifespan(FastAPI()):
        pass
    assert excinfo.value.holder is not None
    assert "add-backend" in excinfo.value.holder.purpose
  finally:
    apply_fence.release()
  # After the apply releases, startup works.
  async with server_module.lifespan(FastAPI()):
    pass


# ---------------------------------------------------------------------------
# Fence failure paths: ownership survives every failure mode
# ---------------------------------------------------------------------------


def test_fence_refuses_symlinked_paths(tmp_path: Path) -> None:
  """A symlinked state directory, lock, or identity record refuses acquisition
  and probing instead of placing or reading the exclusion outside the home."""
  from src.runtime.home_writer_fence import FencePathRefusalError
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


_SHUTDOWN_TIMING_FIELDS = (
    "shutdown_ms",
    "speech_ms",
    "usage_tally_ms",
    "slack_listener_ms",
    "slack_backfill_ms",
    "ext_usage_ms",
    "host_auth_ms",
    "http_client_ms",
    "scheduler_ms",
    "ws_close_ms",
    "merge_pool_ms",
)


class _LogRecorder:
  """A stand-in for the server module's structlog logger, recording every call."""

  def __init__(self) -> None:
    self.events: list[tuple[str, dict]] = []

  def info(self, event: str, **kwargs: object) -> None:
    self.events.append((event, kwargs))

  def __getattr__(self, level: str):

    def _record(event: str, **kwargs: object) -> None:
      self.events.append((event, kwargs))

    return _record


@pytest.mark.asyncio
async def test_lifespan_shutdown_line_carries_the_step_timings(lifespan_env, monkeypatch) -> None:
  """The charliebot_shutdown line names every shutdown step's wall time in
  integer milliseconds, so a slow stop names its slow step."""
  from fastapi import FastAPI

  server_module = lifespan_env
  recorder = _LogRecorder()
  monkeypatch.setattr(server_module, "log", recorder)
  async with server_module.lifespan(FastAPI()):
    pass
  line = next(kwargs for event, kwargs in recorder.events if event == "charliebot_shutdown")
  for name in _SHUTDOWN_TIMING_FIELDS:
    value = line[name]  # a missing field is the failure, not a default
    assert isinstance(
        value, int) and not isinstance(value, bool) and value >= 0, (f"{name}={value!r} is not a non-negative int")


@pytest.mark.asyncio
async def test_server_lifespan_releases_fence_on_shutdown_failure(lifespan_env, monkeypatch) -> None:
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
