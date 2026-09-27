"""Unit tests for native SLURM-job watching via sacct polling."""
from __future__ import annotations

import asyncio
from pathlib import Path
from unittest.mock import patch

import pytest
from conftest import (
    TRIGGERS_SACCT_AVAILABLE_PATCH_TARGET,
    assert_trigger_fired,
    make_sacct_mock,
    patch_trigger_fire,
)
from conftest import make_trigger_setup as _make_mgr
from conftest import no_sleep as _no_sleep

from src.core.models import (
    SlurmJob,
)

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
  sacct = make_sacct_mock({(None, job_id): sacct_lines})

  with patch_trigger_fire(sacct, sacct_available=True, sleep_mock=_no_sleep) as mock_master:
    trigger = await trigger_mgr.create_trigger(
        session_id,
        delay_seconds=600,
        message=message,
        watch_targets=[SlurmJob(job_id=job_id)],
    )
    await asyncio.wait_for(trigger_mgr._tasks[trigger.id], timeout=10)

  msg = await assert_trigger_fired(trigger_mgr, session_id, trigger.id, mock_master, reason="completed")
  assert sacct.call_count >= min_polls
  assert final_line in msg


# ---------------------------------------------------------------------------
# Mixed-kind AND: local pid + slurm job


# ---------------------------------------------------------------------------
# No-sacct host: create-time fail-loud; pure pid / pure delay still work
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_no_sacct_host_slurm_create_fails(tmp_path: Path) -> None:
  _, _, trigger_mgr, session_id = await _make_mgr(tmp_path)
  with (
      patch(TRIGGERS_SACCT_AVAILABLE_PATCH_TARGET, new=False),
      pytest.raises(RuntimeError, match="sacct unavailable"),
  ):
    await trigger_mgr.create_trigger(
        session_id,
        delay_seconds=600,
        message="no slurm here",
        watch_targets=[SlurmJob(job_id=12345)],
    )


# ---------------------------------------------------------------------------
# Recovery: a persisted slurm trigger on a host without sacct skips (no spin)
