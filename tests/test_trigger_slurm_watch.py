"""Unit tests for native SLURM-job watching via sacct polling."""
from __future__ import annotations

import asyncio
import subprocess
import sys
import time
from datetime import UTC, datetime
from pathlib import Path
from unittest.mock import AsyncMock, patch

import pytest
from conftest import (
    TRIGGERS_SACCT_AVAILABLE_PATCH_TARGET,
    FakeAsyncProcess,
    assert_trigger_fired_completed,
    patch_trigger_fire,
)
from conftest import make_trigger_setup as _make_mgr
from conftest import no_sleep as _no_sleep

from src.core.models import (
    LocalPid,
    PendingTrigger,
    SlurmJob,
)
from src.core.triggers import TriggerManager

# ---------------------------------------------------------------------------
# Mock helpers
# ---------------------------------------------------------------------------


def _mk_sacct_mock(outputs: list[str]) -> AsyncMock:
  """Mock ``asyncio.create_subprocess_exec`` returning successive sacct stdouts.

  Each call pops the next entry; the last entry repeats indefinitely.
  """
  queue = list(outputs)

  async def _factory(*args, **kwargs):
    out = queue[0] if len(queue) == 1 else queue.pop(0)
    return FakeAsyncProcess(stdout=out.encode())

  return AsyncMock(side_effect=_factory)


# ---------------------------------------------------------------------------
# Single slurm job: terminal-state detection
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
@pytest.mark.parametrize(
    ("sacct_lines", "message", "job_id", "final_line", "min_polls"), [
        pytest.param(
            ["12345|RUNNING|0:0\n", "12345|COMPLETED|0:0\n"],
            "slurm done",
            12345,
            "finished: slurm:12345: COMPLETED 0:0",
            1,
            id="terminal_completed",
        ),
        pytest.param(
            ["42|FAILED|1:0\n"],
            "slurm failed",
            42,
            "finished: slurm:42: FAILED 1:0",
            1,
            id="failed_captures_exit_code",
        ),
        pytest.param(
            ["7|CANCELLED by 1000|0:15\n"],
            "cancelled",
            7,
            "finished: slurm:7: CANCELLED by 1000 0:15",
            1,
            id="cancelled_uid_suffix_is_terminal",
        ),
        pytest.param(
            ["", "12345|COMPLETED|0:0\n"],
            "lagging",
            12345,
            "finished: slurm:12345: COMPLETED 0:0",
            2,
            id="accounting_lag_keeps_polling",
        ),
        pytest.param(
            ["12345|WEIRD_STATE|0:0\n", "12345|COMPLETED|0:0\n"],
            "weird then done",
            12345,
            "finished: slurm:12345: COMPLETED 0:0",
            2,
            id="unknown_state_keeps_polling",
        ),
    ])
async def test_slurm_single_job_terminal_state(
    tmp_path: Path,
    sacct_lines: list[str],
    message: str,
    job_id: int,
    final_line: str,
    min_polls: int,
) -> None:
  """A single watched slurm job fires completed once sacct reports a terminal state.

  CANCELLED keeps its "by <uid>" suffix verbatim in the fired message. An empty
  answer (accounting lag) and an unknown state are not terminal: the probe keeps
  polling until a terminal state arrives, which min_polls pins for those rows.
  """
  _, _, trigger_mgr, session_id = await _make_mgr(tmp_path)
  sacct = _mk_sacct_mock(sacct_lines)

  with patch_trigger_fire(sacct, sacct_available=True, sleep_mock=_no_sleep) as mock_master:
    trigger = await trigger_mgr.create_trigger(
        session_id,
        delay_seconds=600,
        message=message,
        watch_targets=[SlurmJob(job_id=job_id)],
    )
    await asyncio.wait_for(trigger_mgr._tasks[trigger.id], timeout=10)

  msg = await assert_trigger_fired_completed(trigger_mgr, session_id, trigger.id, mock_master)
  assert sacct.call_count >= min_polls
  assert final_line in msg


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
  msg = await assert_trigger_fired_completed(trigger_mgr, session_id, trigger.id, mock_master)
  assert str(proc.pid) in msg
  assert "slurm:77: COMPLETED 0:0" in msg


# ---------------------------------------------------------------------------
# No-sacct host: create-time fail-loud; pure pid / pure delay still work
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_no_sacct_host_slurm_create_fails(tmp_path: Path) -> None:
  _, _, trigger_mgr, session_id = await _make_mgr(tmp_path)
  with (
      patch(TRIGGERS_SACCT_AVAILABLE_PATCH_TARGET, False),
      pytest.raises(RuntimeError, match="sacct unavailable"),
  ):
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
      patch(TRIGGERS_SACCT_AVAILABLE_PATCH_TARGET, False),
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
      fire_at=datetime.now(UTC),  # already due — return immediately
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
