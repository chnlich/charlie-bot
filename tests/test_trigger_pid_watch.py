"""Unit tests for schedule_trigger optional PID watching."""
from __future__ import annotations

import asyncio
import os
import subprocess
import sys
import time
from pathlib import Path
from unittest.mock import AsyncMock, patch

import pytest
from conftest import make_trigger_setup as _make_mgr

from src.core.models import (
  LocalPid,
  PendingTrigger,
  TriggerStatus,
)
from src.core.triggers import _format_suffix


def _local(*pids: int) -> list[LocalPid]:
  return [LocalPid(pid=p) for p in pids]


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
        watch_targets=_local(missing_pid),
    )
    task = trigger_mgr._tasks[trigger.id]
    await asyncio.wait_for(task, timeout=5)

  stored = await trigger_mgr._load_trigger(session_id, trigger.id)
  assert stored.status == TriggerStatus.FIRED
  assert stored.fire_reason == "completed"
  msg = mock_master.await_args.args[1]
  assert "[Scheduled trigger fired | completed]" in msg
  assert f"finished: {missing_pid} (gone at start)" in msg


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
        watch_targets=_local(proc.pid),
    )
    task = trigger_mgr._tasks[trigger.id]
    start = time.monotonic()
    await asyncio.wait_for(task, timeout=10)
    elapsed = time.monotonic() - start

  proc.wait(timeout=2)
  assert elapsed < 5, f"trigger took {elapsed:.1f}s, expected <5s"

  stored = await trigger_mgr._load_trigger(session_id, trigger.id)
  assert stored.status == TriggerStatus.FIRED
  assert stored.fire_reason == "completed"
  msg = mock_master.await_args.args[1]
  assert "[Scheduled trigger fired | completed]" in msg
  assert f"finished: {proc.pid}" in msg


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
          watch_targets=_local(proc.pid),
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
          watch_targets=_local(fast.pid, slow.pid),
      )
      task = trigger_mgr._tasks[trigger.id]
      start = time.monotonic()
      await asyncio.wait_for(task, timeout=10)
      elapsed = time.monotonic() - start

    assert elapsed >= 1.0, f"fired too early: {elapsed:.2f}s"
    assert elapsed < 5, f"fired too late: {elapsed:.2f}s"
    stored = await trigger_mgr._load_trigger(session_id, trigger.id)
    assert stored.fire_reason == "completed"
    msg = mock_master.await_args.args[1]
    assert "[Scheduled trigger fired | completed]" in msg
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
  assert stored.watch_targets == []


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
          session_id, delay_seconds=10, message="live", watch_targets=_local(proc.pid),
      )
      fresh = None
      for _ in range(50):
        await asyncio.sleep(0.1)
        fresh = await trigger_mgr._load_trigger(session_id, trigger.id)
        if fresh.status == TriggerStatus.FIRED:
          break
      assert fresh is not None and fresh.status == TriggerStatus.FIRED
      assert fresh.fire_reason == "completed"
  finally:
    proc.wait()


def test_format_suffix_completed_gone_at_start() -> None:
  assert _format_suffix("completed", ["111 (gone at start)", "222"], []) == (
      " (finished: 111 (gone at start), 222)")


def test_format_suffix_completed_pids() -> None:
  assert _format_suffix("completed", ["111", "222"], []) == " (finished: 111, 222)"


def test_format_suffix_timeout_with_finished() -> None:
  out = _format_suffix("timeout", ["111"], ["222", "333"])
  assert out == " (finished: 111; still alive: 222, 333)"


def test_format_suffix_timeout_all_alive() -> None:
  out = _format_suffix("timeout", [], ["222", "333"])
  assert out == " (still alive: 222, 333)"


def test_format_suffix_completed_remote_only() -> None:
  out = _format_suffix("completed", ["neptune:5678", "noire:9012"], [])
  assert out == " (finished: neptune:5678, noire:9012)"


def test_format_suffix_completed_slurm() -> None:
  out = _format_suffix("completed", ["slurm:42: COMPLETED 0:0"], [])
  assert out == " (finished: slurm:42: COMPLETED 0:0)"


def test_format_suffix_timeout_mixed_kinds() -> None:
  out = _format_suffix("timeout", ["neptune:5678", "slurm:42: COMPLETED 0:0"], ["1234", "slurm:99"])
  assert out == " (finished: neptune:5678, slurm:42: COMPLETED 0:0; still alive: 1234, slurm:99)"


def test_load_legacy_trigger_without_watch_pids() -> None:
  """An existing JSON file without watch_pids/fire_reason must still load."""
  legacy = (
      '{"id": "legacy-1", "session_id": "sess", '
      '"fire_at": "2030-01-01T00:00:00+00:00", "message": "hi", '
      '"created_at": "2030-01-01T00:00:00+00:00", "status": "pending", '
      '"fired_at": null}'
  )
  trigger = PendingTrigger.model_validate_json(legacy)
  assert trigger.watch_targets == []
  assert trigger.fire_reason is None
