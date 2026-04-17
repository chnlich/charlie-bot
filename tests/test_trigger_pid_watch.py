"""Unit tests for schedule_trigger optional PID watching."""
from __future__ import annotations

import asyncio
import os
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path
from unittest.mock import AsyncMock, patch

import pytest

from src.core.config import CharlieBotConfig
from src.core.models import CreateSessionRequest, PendingTrigger, TriggerStatus
from src.core.sessions import SessionManager
from src.core.triggers import TriggerManager, _format_suffix


@pytest.fixture
def pidfd_open_available() -> None:
  """Skip when the production pidfd helpers are not supported on this host."""
  from src.core.triggers import _PIDFD_SUPPORTED
  if not _PIDFD_SUPPORTED:
    pytest.skip("pidfd not supported on this host (not even via syscall)")


def _find_unused_pid() -> int:
  """Find a PID that is very likely not in use."""
  for candidate in range(4194303, 3999999, -1):
    try:
      os.kill(candidate, 0)
    except ProcessLookupError:
      return candidate
    except PermissionError:
      continue
  raise RuntimeError("could not find an unused PID")


async def _make_mgr(tmp_path: Path) -> tuple[CharlieBotConfig, SessionManager, TriggerManager, str]:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "charliebot-home")
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="Watch PIDs"))
  trigger_mgr = TriggerManager(cfg, session_mgr)
  return cfg, session_mgr, trigger_mgr, session.id


@pytest.mark.asyncio
async def test_pid_gone_immediate_fire(tmp_path: Path, pidfd_open_available: None) -> None:
  _, _, trigger_mgr, session_id = await _make_mgr(tmp_path)
  missing_pid = _find_unused_pid()

  with (
      patch("src.core.sessions.streaming_manager.broadcast", new=AsyncMock()),
      patch("src.core.triggers.trigger_master", new=AsyncMock()) as mock_master,
  ):
    trigger = await trigger_mgr.create_trigger(
        session_id,
        delay_seconds=30,
        message="watch gone",
        watch_pids=[missing_pid],
    )
    task = trigger_mgr._tasks[trigger.id]
    await asyncio.wait_for(task, timeout=5)

  stored = await trigger_mgr._load_trigger(session_id, trigger.id)
  assert stored.status == TriggerStatus.FIRED
  assert stored.fire_reason == "pid_gone"
  msg = mock_master.await_args.args[1]
  assert "[Scheduled trigger fired | pid_gone]" in msg
  assert f"pid_gone: {missing_pid}" in msg


@pytest.mark.asyncio
async def test_pid_exit_before_timeout(tmp_path: Path, pidfd_open_available: None) -> None:
  _, _, trigger_mgr, session_id = await _make_mgr(tmp_path)

  proc = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(0.5)"])

  with (
      patch("src.core.sessions.streaming_manager.broadcast", new=AsyncMock()),
      patch("src.core.triggers.trigger_master", new=AsyncMock()) as mock_master,
  ):
    trigger = await trigger_mgr.create_trigger(
        session_id,
        delay_seconds=30,
        message="watch exit",
        watch_pids=[proc.pid],
    )
    task = trigger_mgr._tasks[trigger.id]
    start = time.monotonic()
    await asyncio.wait_for(task, timeout=10)
    elapsed = time.monotonic() - start

  proc.wait(timeout=2)
  assert elapsed < 5, f"trigger took {elapsed:.1f}s, expected <5s"

  stored = await trigger_mgr._load_trigger(session_id, trigger.id)
  assert stored.status == TriggerStatus.FIRED
  assert stored.fire_reason == "pid_exit"
  msg = mock_master.await_args.args[1]
  assert "[Scheduled trigger fired | pid_exit]" in msg
  assert f"exited: {proc.pid}=" in msg


@pytest.mark.asyncio
async def test_timeout_before_pid_exit(tmp_path: Path, pidfd_open_available: None) -> None:
  _, _, trigger_mgr, session_id = await _make_mgr(tmp_path)

  proc = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"])
  try:
    with (
        patch("src.core.sessions.streaming_manager.broadcast", new=AsyncMock()),
        patch("src.core.triggers.trigger_master", new=AsyncMock()) as mock_master,
    ):
      trigger = await trigger_mgr.create_trigger(
          session_id,
          delay_seconds=1,
          message="watch timeout",
          watch_pids=[proc.pid],
      )
      task = trigger_mgr._tasks[trigger.id]
      await asyncio.wait_for(task, timeout=10)

    stored = await trigger_mgr._load_trigger(session_id, trigger.id)
    assert stored.status == TriggerStatus.FIRED
    assert stored.fire_reason == "timeout"
    msg = mock_master.await_args.args[1]
    assert "[Scheduled trigger fired | timeout]" in msg
    assert f"still alive: {proc.pid}" in msg
  finally:
    proc.kill()
    proc.wait(timeout=5)


@pytest.mark.asyncio
async def test_multiple_pids_all_semantics(tmp_path: Path, pidfd_open_available: None) -> None:
  _, _, trigger_mgr, session_id = await _make_mgr(tmp_path)

  fast = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(0.3)"])
  slow = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(1.5)"])

  try:
    with (
        patch("src.core.sessions.streaming_manager.broadcast", new=AsyncMock()),
        patch("src.core.triggers.trigger_master", new=AsyncMock()) as mock_master,
    ):
      trigger = await trigger_mgr.create_trigger(
          session_id,
          delay_seconds=30,
          message="watch all",
          watch_pids=[fast.pid, slow.pid],
      )
      task = trigger_mgr._tasks[trigger.id]
      start = time.monotonic()
      await asyncio.wait_for(task, timeout=10)
      elapsed = time.monotonic() - start

    assert elapsed >= 1.0, f"fired too early: {elapsed:.2f}s"
    assert elapsed < 5, f"fired too late: {elapsed:.2f}s"
    stored = await trigger_mgr._load_trigger(session_id, trigger.id)
    assert stored.fire_reason == "pid_exit"
    msg = mock_master.await_args.args[1]
    assert "[Scheduled trigger fired | pid_exit]" in msg
    assert str(fast.pid) in msg
    assert str(slow.pid) in msg
  finally:
    for p in (fast, slow):
      if p.poll() is None:
        p.kill()
      p.wait(timeout=5)


@pytest.mark.asyncio
async def test_time_only_path_unchanged(tmp_path: Path) -> None:
  _, _, trigger_mgr, session_id = await _make_mgr(tmp_path)

  with (
      patch("src.core.sessions.streaming_manager.broadcast", new=AsyncMock()),
      patch("src.core.triggers.trigger_master", new=AsyncMock()) as mock_master,
  ):
    trigger = await trigger_mgr.create_trigger(
        session_id,
        delay_seconds=0,
        message="hello",
    )
    task = trigger_mgr._tasks[trigger.id]
    await asyncio.wait_for(task, timeout=5)

  stored = await trigger_mgr._load_trigger(session_id, trigger.id)
  assert stored.status == TriggerStatus.FIRED
  assert stored.fire_reason == "timeout"
  msg = mock_master.await_args.args[1]
  assert msg == "[Scheduled trigger fired] hello"
  assert stored.watch_pids is None


@pytest.mark.asyncio
async def test_pidfd_fallback_works_on_host(tmp_path: Path) -> None:
  from src.core.triggers import _PIDFD_SUPPORTED
  if not _PIDFD_SUPPORTED:
    pytest.skip("pidfd not supported on this host (not even via syscall)")
  proc = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(0.5)"])
  try:
    cfg, session_mgr, trigger_mgr, session_id = await _make_mgr(tmp_path)
    with patch("src.core.triggers.trigger_master", new=AsyncMock()):
      trigger = await trigger_mgr.create_trigger(
          session_id, delay_seconds=10, message="live", watch_pids=[proc.pid],
      )
      fresh = None
      for _ in range(50):
        await asyncio.sleep(0.1)
        fresh = await trigger_mgr._load_trigger(session_id, trigger.id)
        if fresh.status == TriggerStatus.FIRED:
          break
      assert fresh is not None and fresh.status == TriggerStatus.FIRED
      assert fresh.fire_reason == "pid_exit"
  finally:
    proc.wait()


def test_format_suffix_pid_gone() -> None:
  assert _format_suffix("pid_gone", [], [], [111, 222]) == " (pid_gone: 111, 222)"


def test_format_suffix_pid_exit_with_unknown() -> None:
  out = _format_suffix("pid_exit", [(111, 0), (222, None)], [], [])
  assert out == " (exited: 111=0, 222=unknown)"


def test_format_suffix_timeout_with_exited() -> None:
  out = _format_suffix("timeout", [(111, 0)], [222, 333], [])
  assert out == " (exited: 111=0; still alive: 222, 333)"


def test_format_suffix_timeout_all_alive() -> None:
  out = _format_suffix("timeout", [], [222, 333], [])
  assert out == " (still alive: 222, 333)"


def test_backwards_compat_load_legacy_trigger() -> None:
  """An existing JSON file without watch_pids/fire_reason must still load."""
  legacy = (
      '{"id": "legacy-1", "session_id": "sess", '
      '"fire_at": "2030-01-01T00:00:00+00:00", "message": "hi", '
      '"created_at": "2030-01-01T00:00:00+00:00", "status": "pending", '
      '"fired_at": null}'
  )
  trigger = PendingTrigger.model_validate_json(legacy)
  assert trigger.watch_pids is None
  assert trigger.fire_reason is None
