"""Unit tests for remote SLURM-job watching via ssh-polled sacct (slice 1a)."""
from __future__ import annotations

import asyncio
from pathlib import Path
from unittest.mock import AsyncMock, patch

import pytest

from src.core.config import CharlieBotConfig
from src.core.models import CreateSessionRequest, SlurmJob, TriggerStatus
from src.core.sessions import SessionManager
from src.core.triggers import TriggerManager, _probe_sacct


# ---------------------------------------------------------------------------
# Mock helpers
# ---------------------------------------------------------------------------


class _FakeProc:
  """Stand-in for asyncio.subprocess.Process."""

  def __init__(self, stdout: bytes, stderr: bytes = b"", returncode: int = 0) -> None:
    self._stdout = stdout
    self._stderr = stderr
    self.returncode = returncode
    self.kill_called = False

  async def communicate(self) -> tuple[bytes, bytes]:
    return self._stdout, self._stderr

  def kill(self) -> None:
    self.kill_called = True

  async def wait(self) -> int:
    return self.returncode


def _mk_sacct_mock(scripted: dict[tuple[str | None, int], list[str]]) -> AsyncMock:
  """Build a mock for ``asyncio.create_subprocess_exec`` returning sacct stdouts.

  Inspects argv to identify the probe: a local sacct call
  (``sacct -j ID ...``) or a remote ssh call
  (``ssh ... HOST "sacct -j ID ..."``). ``scripted`` maps ``(host, job_id)`` to a
  list of sacct stdout payloads (host is None for local probes); each call pops
  the next entry, and the last entry repeats indefinitely.
  """
  queues: dict[tuple[str | None, int], list[str]] = {k: list(v) for k, v in scripted.items()}

  async def _factory(*args, **kwargs):  # noqa: ARG001
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
    return _FakeProc(stdout=out.encode())

  return AsyncMock(side_effect=_factory)


async def _no_sleep(_seconds: float) -> None:
  return None


async def _make_mgr(tmp_path: Path) -> tuple[CharlieBotConfig, SessionManager, TriggerManager, str]:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "charliebot-home")
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="Remote watch"))
  trigger_mgr = TriggerManager(cfg, session_mgr)
  return cfg, session_mgr, trigger_mgr, session.id


# ---------------------------------------------------------------------------
# Remote SLURM watch: completion and timeout
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_remote_slurm_completes(tmp_path: Path) -> None:
  _, _, trigger_mgr, session_id = await _make_mgr(tmp_path)
  sacct = _mk_sacct_mock({("host2", 122111): ["122111|COMPLETED|0:0\n"]})

  with (
      patch("src.core.triggers._SACCT_AVAILABLE", False),
      patch("src.core.triggers.asyncio.create_subprocess_exec", new=sacct),
      patch("src.core.triggers.asyncio.sleep", new=_no_sleep),
      patch("src.core.sessions.streaming_manager.broadcast", new=AsyncMock()),
      patch("src.core.triggers.trigger_master", new=AsyncMock()) as mock_master,
  ):
    trigger = await trigger_mgr.create_trigger(
        session_id,
        delay_seconds=600,
        message="remote slurm done",
        watch_targets=[SlurmJob(host="host2", job_id=122111)],
    )
    await asyncio.wait_for(trigger_mgr._tasks[trigger.id], timeout=10)

  stored = await trigger_mgr._load_trigger(session_id, trigger.id)
  assert stored.status == TriggerStatus.FIRED
  assert stored.fire_reason == "completed"
  msg = mock_master.await_args.args[1]
  assert "[Scheduled trigger fired | completed]" in msg
  assert "finished: host2:slurm:122111: COMPLETED 0:0" in msg


@pytest.mark.asyncio
async def test_remote_slurm_timeout_while_running(tmp_path: Path) -> None:
  _, _, trigger_mgr, session_id = await _make_mgr(tmp_path)
  sacct = _mk_sacct_mock({("host2", 122111): ["122111|RUNNING|0:0\n"]})

  with (
      patch("src.core.triggers._SACCT_AVAILABLE", False),
      patch("src.core.triggers.asyncio.create_subprocess_exec", new=sacct),
      patch("src.core.triggers.asyncio.sleep", new=_no_sleep),
      patch("src.core.sessions.streaming_manager.broadcast", new=AsyncMock()),
      patch("src.core.triggers.trigger_master", new=AsyncMock()) as mock_master,
  ):
    trigger = await trigger_mgr.create_trigger(
        session_id,
        delay_seconds=0,  # fire_at already due — one probe, then timeout
        message="still running",
        watch_targets=[SlurmJob(host="host2", job_id=122111)],
    )
    await asyncio.wait_for(trigger_mgr._tasks[trigger.id], timeout=10)

  stored = await trigger_mgr._load_trigger(session_id, trigger.id)
  assert stored.status == TriggerStatus.FIRED
  assert stored.fire_reason == "timeout"
  msg = mock_master.await_args.args[1]
  assert "[Scheduled trigger fired | timeout]" in msg
  assert "still alive: host2:slurm:122111" in msg


# ---------------------------------------------------------------------------
# sacct row parsing: array-task rows are rejected by shape, not by int()
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_probe_sacct_skips_array_task_rows() -> None:
  sacct = _mk_sacct_mock({(None, 122111): ["122111_3|COMPLETED|0:0\n122111|RUNNING|0:0\n"]})

  with patch("src.core.triggers.asyncio.create_subprocess_exec", new=sacct):
    states, error = await _probe_sacct([122111], "trig-test", host=None)

  assert error is None
  assert 122111 in states
  assert 1221113 not in states
