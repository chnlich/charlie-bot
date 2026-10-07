"""Unit tests for remote SLURM-job watching via ssh-polled sacct (slice 1a)."""
from __future__ import annotations

import asyncio
import pathlib
from typing import Any
from unittest import mock

import conftest
import pytest

from src.infra import models

# ---------------------------------------------------------------------------
# Remote SLURM watch: completion and timeout
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_remote_slurm_completes(tmp_path: pathlib.Path) -> None:
  _, _, trigger_mgr, session_id = await conftest.make_trigger_setup(tmp_path)
  sacct = conftest.make_sacct_mock({("host2", 122111): ["122111|COMPLETED|0:0\n"]})

  with conftest.patch_trigger_fire(sacct, sacct_available=False, sleep_mock=conftest.no_sleep) as mock_master:
    trigger = await trigger_mgr.create_trigger(
        session_id,
        delay_seconds=600,
        message="remote slurm done",
        watch_targets=[models.SlurmJob(host="host2", job_id=122111)],
    )
    await asyncio.wait_for(trigger_mgr._tasks[trigger.id], timeout=10)

  msg = await conftest.assert_trigger_fired(trigger_mgr, session_id, trigger.id, mock_master, reason="completed")
  assert "finished: host2:slurm:122111: COMPLETED 0:0" in msg


# ---------------------------------------------------------------------------
# Unreachable host: silent for the grace window -> fire early via still_alive
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_unreachable_host_fires_early_with_note(tmp_path: pathlib.Path) -> None:
  _, _, trigger_mgr, session_id = await conftest.make_trigger_setup(tmp_path)
  calls = [0]

  async def _factory(*args: Any, **kwargs: Any) -> conftest.FakeAsyncProcess:
    calls[0] += 1
    if calls[0] == 1:
      # verify-on-create succeeds so the trigger is persisted and the wait task starts
      return conftest.FakeAsyncProcess(stdout=b"122111|RUNNING|0:0\n")
    # every subsequent wait probe fails -> the host goes dark and is escalated
    return conftest.FakeAsyncProcess(stdout=b"", stderr=b"ssh: Connection timed out", returncode=255)

  with (
      conftest.patch_trigger_fire(mock.AsyncMock(side_effect=_factory), sacct_available=False,
                                  sleep_mock=conftest.no_sleep) as mock_master,
      mock.patch("src.runtime.triggers._REMOTE_SACCT_UNREACHABLE_GRACE", 0),
  ):
    trigger = await trigger_mgr.create_trigger(
        session_id,
        delay_seconds=3600,  # large: a plain timeout cannot explain an early fire
        message="unreachable host",
        watch_targets=[models.SlurmJob(host="host2", job_id=122111)],
    )
    await asyncio.wait_for(trigger_mgr._tasks[trigger.id], timeout=10)

  msg = await conftest.assert_trigger_fired(trigger_mgr, session_id, trigger.id, mock_master, reason="timeout")
  assert "host2:slurm:122111 (unreachable " in msg
  stored = await trigger_mgr._load_trigger(session_id, trigger.id)
  assert stored.fired_at < trigger.fire_at
  assert (trigger.fire_at - stored.fired_at).total_seconds() > 3000
