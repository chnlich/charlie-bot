"""Unit tests for native SLURM-job watching via sacct polling."""
from __future__ import annotations

import asyncio
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path
from unittest.mock import AsyncMock, patch

import pytest
from conftest import make_trigger_setup as _make_mgr
from conftest import no_sleep as _no_sleep
from conftest import patch_trigger_fire

from src.core.models import (
  LocalPid,
  PendingTrigger,
  SlurmJob,
  TriggerStatus,
)
from src.core.triggers import TriggerManager

# ---------------------------------------------------------------------------
# Mock helpers
# ---------------------------------------------------------------------------


class _FakeProc:
  """Stand-in for asyncio.subprocess.Process running sacct."""

  def __init__(self, stdout: bytes, stderr: bytes = b"", returncode: int = 0) -> None:
    self._stdout = stdout
    self._stderr = stderr
    self.returncode = returncode

  async def communicate(self) -> tuple[bytes, bytes]:
    return self._stdout, self._stderr


def _mk_sacct_mock(outputs: list[str]) -> AsyncMock:
  """Mock ``asyncio.create_subprocess_exec`` returning successive sacct stdouts.

  Each call pops the next entry; the last entry repeats indefinitely.
  """
  queue = list(outputs)

  async def _factory(*args, **kwargs):
    out = queue[0] if len(queue) == 1 else queue.pop(0)
    return _FakeProc(stdout=out.encode())

  return AsyncMock(side_effect=_factory)


@pytest.fixture
def pidfd_open_available() -> None:
  """Skip when pidfd helpers are unavailable (needed for mixed local+slurm)."""
  from src.core.triggers import _PIDFD_SUPPORTED
  if not _PIDFD_SUPPORTED:
    pytest.skip("pidfd not supported on this host")


# ---------------------------------------------------------------------------
# Single slurm job: terminal-state detection
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_slurm_single_job_completes(tmp_path: Path) -> None:
  _, _, trigger_mgr, session_id = await _make_mgr(tmp_path)
  sacct = _mk_sacct_mock(["12345|RUNNING|0:0\n", "12345|COMPLETED|0:0\n"])

  with patch_trigger_fire(sacct, sacct_available=True, sleep_mock=_no_sleep) as mock_master:
    trigger = await trigger_mgr.create_trigger(
        session_id,
        delay_seconds=600,
        message="slurm done",
        watch_targets=[SlurmJob(job_id=12345)],
    )
    task = trigger_mgr._tasks[trigger.id]
    await asyncio.wait_for(task, timeout=10)

  stored = await trigger_mgr._load_trigger(session_id, trigger.id)
  assert stored.status == TriggerStatus.FIRED
  assert stored.fire_reason == "completed"
  msg = mock_master.await_args.args[1]
  assert "[Scheduled trigger fired | completed]" in msg
  assert "finished: slurm:12345: COMPLETED 0:0" in msg


@pytest.mark.asyncio
async def test_slurm_failed_captures_exit_code(tmp_path: Path) -> None:
  _, _, trigger_mgr, session_id = await _make_mgr(tmp_path)
  sacct = _mk_sacct_mock(["42|FAILED|1:0\n"])

  with patch_trigger_fire(sacct, sacct_available=True, sleep_mock=_no_sleep) as mock_master:
    trigger = await trigger_mgr.create_trigger(
        session_id,
        delay_seconds=600,
        message="slurm failed",
        watch_targets=[SlurmJob(job_id=42)],
    )
    await asyncio.wait_for(trigger_mgr._tasks[trigger.id], timeout=10)

  stored = await trigger_mgr._load_trigger(session_id, trigger.id)
  assert stored.fire_reason == "completed"
  assert "finished: slurm:42: FAILED 1:0" in mock_master.await_args.args[1]


@pytest.mark.asyncio
async def test_slurm_cancelled_with_uid_suffix_is_terminal(tmp_path: Path) -> None:
  """A 'CANCELLED by <uid>' state still resolves to the terminal CANCELLED."""
  _, _, trigger_mgr, session_id = await _make_mgr(tmp_path)
  sacct = _mk_sacct_mock(["7|CANCELLED by 1000|0:15\n"])

  with patch_trigger_fire(sacct, sacct_available=True, sleep_mock=_no_sleep) as mock_master:
    trigger = await trigger_mgr.create_trigger(
        session_id,
        delay_seconds=600,
        message="cancelled",
        watch_targets=[SlurmJob(job_id=7)],
    )
    await asyncio.wait_for(trigger_mgr._tasks[trigger.id], timeout=10)

  stored = await trigger_mgr._load_trigger(session_id, trigger.id)
  assert stored.fire_reason == "completed"
  assert "finished: slurm:7: CANCELLED by 1000 0:15" in mock_master.await_args.args[1]


@pytest.mark.asyncio
async def test_slurm_timeout_while_still_active(tmp_path: Path) -> None:
  _, _, trigger_mgr, session_id = await _make_mgr(tmp_path)
  sacct = _mk_sacct_mock(["12345|RUNNING|0:0\n"])  # never leaves RUNNING

  with patch_trigger_fire(sacct, sacct_available=True, sleep_mock=_no_sleep) as mock_master:
    trigger = await trigger_mgr.create_trigger(
        session_id,
        delay_seconds=0,  # fire_at already due — one probe, then timeout
        message="still running",
        watch_targets=[SlurmJob(job_id=12345)],
    )
    await asyncio.wait_for(trigger_mgr._tasks[trigger.id], timeout=10)

  stored = await trigger_mgr._load_trigger(session_id, trigger.id)
  assert stored.fire_reason == "timeout"
  assert "still alive: slurm:12345" in mock_master.await_args.args[1]


# ---------------------------------------------------------------------------
# Accounting lag / unknown state: keep polling, never reject
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_sacct_accounting_lag_keeps_polling(tmp_path: Path) -> None:
  _, _, trigger_mgr, session_id = await _make_mgr(tmp_path)
  # Empty result first (job not yet in accounting), then COMPLETED.
  sacct = _mk_sacct_mock(["", "12345|COMPLETED|0:0\n"])

  with patch_trigger_fire(sacct, sacct_available=True, sleep_mock=_no_sleep) as mock_master:
    trigger = await trigger_mgr.create_trigger(
        session_id,
        delay_seconds=600,
        message="lagging",
        watch_targets=[SlurmJob(job_id=12345)],
    )
    await asyncio.wait_for(trigger_mgr._tasks[trigger.id], timeout=10)

  stored = await trigger_mgr._load_trigger(session_id, trigger.id)
  assert stored.fire_reason == "completed"
  # The empty result was NOT treated as terminal: it kept polling.
  assert sacct.call_count >= 2
  assert "finished: slurm:12345: COMPLETED 0:0" in mock_master.await_args.args[1]


@pytest.mark.asyncio
async def test_sacct_unknown_state_keeps_polling(tmp_path: Path) -> None:
  _, _, trigger_mgr, session_id = await _make_mgr(tmp_path)
  sacct = _mk_sacct_mock(["12345|WEIRD_STATE|0:0\n", "12345|COMPLETED|0:0\n"])

  with patch_trigger_fire(sacct, sacct_available=True, sleep_mock=_no_sleep) as mock_master:
    trigger = await trigger_mgr.create_trigger(
        session_id,
        delay_seconds=600,
        message="weird then done",
        watch_targets=[SlurmJob(job_id=12345)],
    )
    await asyncio.wait_for(trigger_mgr._tasks[trigger.id], timeout=10)

  stored = await trigger_mgr._load_trigger(session_id, trigger.id)
  assert stored.fire_reason == "completed"
  assert sacct.call_count >= 2
  assert "finished: slurm:12345: COMPLETED 0:0" in mock_master.await_args.args[1]


# ---------------------------------------------------------------------------
# Mixed-kind AND: local pid + slurm job
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_mixed_local_and_slurm_and_semantics(tmp_path: Path, pidfd_open_available: None) -> None:
  _, _, trigger_mgr, session_id = await _make_mgr(tmp_path)
  # Slurm completes on the first probe; the local pid outlives it. The trigger
  # must wait for BOTH (AND), so it fires only after the local pid exits.
  proc = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(0.6)"])
  sacct = _mk_sacct_mock(["77|COMPLETED|0:0\n"])

  try:
    with patch_trigger_fire(sacct, sacct_available=True, sleep_mock=None) as mock_master:
      trigger = await trigger_mgr.create_trigger(
          session_id,
          delay_seconds=30,
          message="local + slurm",
          watch_targets=[LocalPid(pid=proc.pid), SlurmJob(job_id=77)],
      )
      start = time.monotonic()
      await asyncio.wait_for(trigger_mgr._tasks[trigger.id], timeout=10)
      elapsed = time.monotonic() - start
  finally:
    proc.wait(timeout=5)

  assert elapsed >= 0.4, f"fired before the local pid exited: {elapsed:.2f}s"
  assert elapsed < 5, f"fired too late: {elapsed:.2f}s"
  stored = await trigger_mgr._load_trigger(session_id, trigger.id)
  assert stored.fire_reason == "completed"
  msg = mock_master.await_args.args[1]
  assert "[Scheduled trigger fired | completed]" in msg
  assert str(proc.pid) in msg
  assert "slurm:77: COMPLETED 0:0" in msg


# ---------------------------------------------------------------------------
# No-sacct host: create-time fail-loud; pure pid / pure delay still work
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_no_sacct_host_slurm_create_fails(tmp_path: Path) -> None:
  _, _, trigger_mgr, session_id = await _make_mgr(tmp_path)
  with patch("src.core.triggers._SACCT_AVAILABLE", False):
    with pytest.raises(RuntimeError, match="sacct unavailable"):
      await trigger_mgr.create_trigger(
          session_id,
          delay_seconds=600,
          message="no slurm here",
          watch_targets=[SlurmJob(job_id=12345)],
      )


@pytest.mark.asyncio
async def test_no_sacct_host_pure_delay_unaffected(tmp_path: Path) -> None:
  _, _, trigger_mgr, session_id = await _make_mgr(tmp_path)
  with (
      patch("src.core.triggers._SACCT_AVAILABLE", False),
      patch.object(TriggerManager, "_start_task", lambda self, t: None),
  ):
    trigger = await trigger_mgr.create_trigger(
        session_id,
        delay_seconds=300,
        message="just a delay",
    )
  assert trigger.watch_targets == []


# ---------------------------------------------------------------------------
# Recovery: a persisted slurm trigger on a host without sacct skips (no spin)
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_recovery_no_sacct_skips_without_spinning(tmp_path: Path) -> None:
  _, _, trigger_mgr, session_id = await _make_mgr(tmp_path)
  trigger = PendingTrigger(
      session_id=session_id,
      fire_at=datetime.now(timezone.utc),  # already due — return immediately
      message="recovered slurm",
      watch_targets=[SlurmJob(job_id=12345)],
  )
  await trigger_mgr._save_trigger(trigger)
  sacct = _mk_sacct_mock(["12345|COMPLETED|0:0\n"])

  with patch_trigger_fire(sacct, sacct_available=False, sleep_mock=None) as mock_master:
    await trigger_mgr._wait_and_fire(trigger)

  # sacct is never invoked (no spin on a missing binary); the trigger times out.
  assert sacct.call_count == 0
  stored = await trigger_mgr._load_trigger(session_id, trigger.id)
  assert stored.fire_reason == "timeout"
  assert "still alive: slurm:12345" in mock_master.await_args.args[1]
