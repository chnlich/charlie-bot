"""Unit tests for remote SLURM-job watching via ssh-polled sacct (slice 1a)."""
from __future__ import annotations

import asyncio
from pathlib import Path
from unittest.mock import AsyncMock, patch

import pytest
from conftest import (
    TRIGGERS_ASYNCIO_CREATE_SUBPROCESS_EXEC_PATCH_TARGET,
    TRIGGERS_SACCT_AVAILABLE_PATCH_TARGET,
    FakeAsyncProcess,
    assert_trigger_fired_completed,
    assert_trigger_fired_timeout,
    patch_trigger_fire,
)
from conftest import make_trigger_setup as _make_mgr
from conftest import no_sleep as _no_sleep

from src.core.models import SlurmJob
from src.core.triggers import RemoteVerifyError, _probe_sacct

# ---------------------------------------------------------------------------
# Mock helpers
# ---------------------------------------------------------------------------


def _mk_sacct_mock(scripted: dict[tuple[str | None, int], list[str]]) -> AsyncMock:
  """Build a mock for ``asyncio.create_subprocess_exec`` returning sacct stdouts.

  Inspects argv to identify the probe: a local sacct call
  (``sacct -j ID ...``) or a remote ssh call
  (``ssh ... HOST "sacct -j ID ..."``). ``scripted`` maps ``(host, job_id)`` to a
  list of sacct stdout payloads (host is None for local probes); each call pops
  the next entry, and the last entry repeats indefinitely.
  """
  queues: dict[tuple[str | None, int], list[str]] = {k: list(v) for k, v in scripted.items()}

  async def _factory(*args, **kwargs):
    if args[0] == "ssh":
      # Layout: ssh -o BatchMode=yes -o ConnectTimeout=10 HOST "sacct -j ID ..."
      host = args[5]
      sacct_cmd = args[6]
      job_id = int(sacct_cmd.split()[2])
    else:
      # Layout: sacct -j ID -X -n -P --format=JobID,State,ExitCode
      host = None
      job_id = int(args[2])
    queue = queues[(host, job_id)]
    out = queue[0] if len(queue) == 1 else queue.pop(0)
    return FakeAsyncProcess(stdout=out.encode())

  return AsyncMock(side_effect=_factory)


# ---------------------------------------------------------------------------
# Remote SLURM watch: completion and timeout
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_remote_slurm_completes(tmp_path: Path) -> None:
  _, _, trigger_mgr, session_id = await _make_mgr(tmp_path)
  sacct = _mk_sacct_mock({("host2", 122111): ["122111|COMPLETED|0:0\n"]})

  with patch_trigger_fire(sacct, sacct_available=False, sleep_mock=_no_sleep) as mock_master:
    trigger = await trigger_mgr.create_trigger(
        session_id,
        delay_seconds=600,
        message="remote slurm done",
        watch_targets=[SlurmJob(host="host2", job_id=122111)],
    )
    await asyncio.wait_for(trigger_mgr._tasks[trigger.id], timeout=10)

  msg = await assert_trigger_fired_completed(trigger_mgr, session_id, trigger.id, mock_master)
  assert "finished: host2:slurm:122111: COMPLETED 0:0" in msg


@pytest.mark.asyncio
async def test_remote_slurm_timeout_while_running(tmp_path: Path) -> None:
  _, _, trigger_mgr, session_id = await _make_mgr(tmp_path)
  sacct = _mk_sacct_mock({("host2", 122111): ["122111|RUNNING|0:0\n"]})

  with patch_trigger_fire(sacct, sacct_available=False, sleep_mock=_no_sleep) as mock_master:
    trigger = await trigger_mgr.create_trigger(
        session_id,
        delay_seconds=0,  # fire_at already due — one probe, then timeout
        message="still running",
        watch_targets=[SlurmJob(host="host2", job_id=122111)],
    )
    await asyncio.wait_for(trigger_mgr._tasks[trigger.id], timeout=10)

  msg = await assert_trigger_fired_timeout(trigger_mgr, session_id, trigger.id, mock_master)
  assert "still alive: host2:slurm:122111" in msg


# ---------------------------------------------------------------------------
# sacct row parsing: array-task rows are rejected by shape, not by int()
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_probe_sacct_skips_array_task_rows() -> None:
  sacct = _mk_sacct_mock({(None, 122111): ["122111_3|COMPLETED|0:0\n122111|RUNNING|0:0\n"]})

  with patch(TRIGGERS_ASYNCIO_CREATE_SUBPROCESS_EXEC_PATCH_TARGET, new=sacct):
    states, error = await _probe_sacct([122111], "trig-test", host=None)

  assert error is None
  assert 122111 in states
  assert 1221113 not in states


# ---------------------------------------------------------------------------
# verify-on-create: probe failure, observed state, accounting lag
# ---------------------------------------------------------------------------


async def _failing_sacct_factory(*args, **kwargs):
  """Always return a failed remote sacct probe (ssh non-zero exit)."""
  return FakeAsyncProcess(
      stdout=b"",
      stderr=b"ssh: connect to host host2 port 22: Connection refused",
      returncode=255,
  )


@pytest.mark.asyncio
async def test_verify_on_create_rejects_failed_probe(tmp_path: Path) -> None:
  cfg, _, trigger_mgr, session_id = await _make_mgr(tmp_path)

  with (
      patch(TRIGGERS_SACCT_AVAILABLE_PATCH_TARGET, False),
      patch(TRIGGERS_ASYNCIO_CREATE_SUBPROCESS_EXEC_PATCH_TARGET, new=AsyncMock(side_effect=_failing_sacct_factory)),
      pytest.raises(RemoteVerifyError),
  ):
    await trigger_mgr.create_trigger(
        session_id,
        delay_seconds=600,
        message="should not persist",
        watch_targets=[SlurmJob(host="host2", job_id=122111)],
    )

  triggers_dir = cfg.sessions_dir / session_id / "triggers"
  assert not list(triggers_dir.glob("*.json"))


@pytest.mark.asyncio
async def test_verify_on_create_reports_observed_state(tmp_path: Path) -> None:
  _, _, trigger_mgr, session_id = await _make_mgr(tmp_path)
  sacct = _mk_sacct_mock({("host2", 122111): ["122111|RUNNING|0:0\n"]})
  probe_out: dict[str, str] = {}

  with patch_trigger_fire(sacct, sacct_available=False, sleep_mock=_no_sleep):
    trigger = await trigger_mgr.create_trigger(
        session_id,
        delay_seconds=0,
        message="observed",
        watch_targets=[SlurmJob(host="host2", job_id=122111)],
        probe_out=probe_out,
    )
    await asyncio.wait_for(trigger_mgr._tasks[trigger.id], timeout=10)

  assert probe_out == {"host2:slurm:122111": "RUNNING"}


@pytest.mark.asyncio
async def test_verify_on_create_reports_not_yet_registered(tmp_path: Path) -> None:
  _, _, trigger_mgr, session_id = await _make_mgr(tmp_path)
  sacct = _mk_sacct_mock({("host2", 122111): [""]})
  probe_out: dict[str, str] = {}

  with patch_trigger_fire(sacct, sacct_available=False, sleep_mock=_no_sleep):
    trigger = await trigger_mgr.create_trigger(
        session_id,
        delay_seconds=0,
        message="not yet registered",
        watch_targets=[SlurmJob(host="host2", job_id=122111)],
        probe_out=probe_out,
    )
    await asyncio.wait_for(trigger_mgr._tasks[trigger.id], timeout=10)

  assert probe_out == {"host2:slurm:122111": "not-yet-registered"}


# ---------------------------------------------------------------------------
# Unreachable host: silent for the grace window -> fire early via still_alive
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_unreachable_host_fires_early_with_note(tmp_path: Path) -> None:
  _, _, trigger_mgr, session_id = await _make_mgr(tmp_path)
  calls = [0]

  async def _factory(*args, **kwargs):
    calls[0] += 1
    if calls[0] == 1:
      # verify-on-create succeeds so the trigger is persisted and the wait task starts
      return FakeAsyncProcess(stdout=b"122111|RUNNING|0:0\n")
    # every subsequent wait probe fails -> the host goes dark and is escalated
    return FakeAsyncProcess(stdout=b"", stderr=b"ssh: Connection timed out", returncode=255)

  with (
      patch_trigger_fire(AsyncMock(side_effect=_factory), sacct_available=False, sleep_mock=_no_sleep) as mock_master,
      patch("src.core.triggers._REMOTE_SACCT_UNREACHABLE_GRACE", 0),
  ):
    trigger = await trigger_mgr.create_trigger(
        session_id,
        delay_seconds=3600,  # large: a plain timeout cannot explain an early fire
        message="unreachable host",
        watch_targets=[SlurmJob(host="host2", job_id=122111)],
    )
    await asyncio.wait_for(trigger_mgr._tasks[trigger.id], timeout=10)

  msg = await assert_trigger_fired_timeout(trigger_mgr, session_id, trigger.id, mock_master)
  assert "host2:slurm:122111 (unreachable " in msg
  stored = await trigger_mgr._load_trigger(session_id, trigger.id)
  assert stored.fired_at < trigger.fire_at
  assert (trigger.fire_at - stored.fired_at).total_seconds() > 3000
