"""Unit tests for remote PID watching via ssh polling (Tool 2)."""
from __future__ import annotations

import asyncio
import json
import pathlib
from typing import Any
from unittest import mock

import conftest
import pytest

from src.infra import models
from src.runtime import triggers

# ---------------------------------------------------------------------------
# Mock helpers
# ---------------------------------------------------------------------------


def _mk_subprocess_mock(scripted: dict[tuple[str, int], list[str]]) -> mock.AsyncMock:
  """Build a mock for ``asyncio.create_subprocess_exec``.

  ``scripted`` maps (host, pid) -> list of statuses ("ALIVE" / "DEAD"). Each
  call pops the next entry; the last entry is repeated indefinitely.
  """

  async def _factory(*args: Any, **kwargs: Any) -> conftest.FakeAsyncProcess:
    # Extract host and `kill -0 PID 2>&1 ...` payload from cmd.
    # Layout: ssh -o <pairs...> HOST "kill -0 PID ..."; the host is the last
    # bare word before the quoted remote command.
    host = args[-2]
    payload = args[-1]
    pid = int(payload.split()[2])
    queue = scripted[(host, pid)]
    status = queue[0] if len(queue) == 1 else queue.pop(0)
    return conftest.FakeAsyncProcess(stdout=(status + "\n").encode())

  return mock.AsyncMock(side_effect=_factory)


# ---------------------------------------------------------------------------
# Verify-on-create
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_remote_create_alive_persists(tmp_path: pathlib.Path) -> None:
  cfg, _, trigger_mgr, session_id = await conftest.make_trigger_setup(tmp_path)
  scripted = {("neptune", 1234): ["ALIVE"]}

  with (
      mock.patch(conftest.TRIGGERS_ASYNCIO_CREATE_SUBPROCESS_EXEC_PATCH_TARGET, new=_mk_subprocess_mock(scripted)),
      mock.patch(conftest.TRIGGER_TASK_DELIVERY_PATCH_TARGET, new=mock.AsyncMock()),
      mock.patch.object(triggers.TriggerManager, "_start_task", lambda self, t: None),
  ):
    trigger = await trigger_mgr.create_trigger(
        session_id,
        delay_seconds=30,
        message="remote watch",
        watch_targets=[models.RemotePid(host="neptune", pid=1234)],
    )

  # File on disk has the new schema.
  raw = (cfg.sessions_dir / session_id / "triggers" / f"{trigger.id}.json").read_text("utf-8")
  data = json.loads(raw)
  assert data["watch_targets"] == [
      {
          "kind": "remote_pid",
          "host": "neptune",
          "pid": 1234,
          "exit_file": None,
          "log": None
      }
  ]
  assert "watch_pids" not in data


@pytest.mark.asyncio
async def test_remote_create_dead_rejects(tmp_path: pathlib.Path) -> None:
  cfg, _, trigger_mgr, session_id = await conftest.make_trigger_setup(tmp_path)
  scripted = {("neptune", 1234): ["DEAD"]}

  with (
      mock.patch(conftest.TRIGGERS_ASYNCIO_CREATE_SUBPROCESS_EXEC_PATCH_TARGET, new=_mk_subprocess_mock(scripted)),
      pytest.raises(triggers.RemoteVerifyError) as excinfo,
  ):
    await trigger_mgr.create_trigger(
        session_id,
        delay_seconds=30,
        message="dead remote",
        watch_targets=[models.RemotePid(host="neptune", pid=1234)],
    )

  assert "neptune:1234" in str(excinfo.value)
  # Nothing was persisted.
  triggers_dir = cfg.sessions_dir / session_id / "triggers"
  assert not triggers_dir.exists() or not list(triggers_dir.glob("*.json"))


# ---------------------------------------------------------------------------
# Fire-time labels: exit-file reads over ssh for launcher-started targets
# ---------------------------------------------------------------------------


def _mk_watched_launch_mock(probe_statuses: list[str], exit_read: tuple[str, int] | None) -> mock.AsyncMock:
  """Mock create_subprocess_exec for a launcher-started remote watch.

  ``kill -0`` probes answer from ``probe_statuses`` (the last entry repeats);
  the ``cat`` exit-file read answers ``exit_read``'s (stdout, returncode).
  """
  queue = list(probe_statuses)

  async def _factory(*args: Any, **kwargs: Any) -> conftest.FakeAsyncProcess:
    payload = args[-1]
    if payload.startswith("kill -0"):
      status = queue[0] if len(queue) == 1 else queue.pop(0)
      return conftest.FakeAsyncProcess(stdout=(status + "\n").encode())
    if payload.startswith("cat "):
      assert exit_read is not None
      stdout, returncode = exit_read
      return conftest.FakeAsyncProcess(stdout=stdout.encode(), returncode=returncode)
    raise AssertionError(f"unexpected ssh payload: {payload!r}")

  return mock.AsyncMock(side_effect=_factory)


@pytest.mark.asyncio
async def test_remote_finished_label_reads_the_exit_file_over_ssh(tmp_path: pathlib.Path) -> None:
  _, _, trigger_mgr, session_id = await conftest.make_trigger_setup(tmp_path)
  subprocess_mock = _mk_watched_launch_mock(["ALIVE", "DEAD"], ("0\n", 0))
  target = models.RemotePid(
      host="neptune", pid=1234, exit_file="/tmp/charliebot_runs/x/exit", log="/tmp/charliebot_runs/x/log")

  with conftest.patch_trigger_fire(subprocess_mock, sacct_available=None, sleep_mock=conftest.no_sleep) as mock_master:
    trigger = await trigger_mgr.create_trigger(
        session_id, delay_seconds=600, message="remote launcher done", watch_targets=[target])
    await asyncio.wait_for(trigger_mgr._tasks[trigger.id], timeout=10)

  msg = await conftest.assert_trigger_fired(trigger_mgr, session_id, trigger.id, mock_master, reason="completed")
  assert "finished: neptune:1234: exit 0, log /tmp/charliebot_runs/x/log" in msg


@pytest.mark.asyncio
async def test_remote_finished_label_exit_file_missing_states_no_code(tmp_path: pathlib.Path) -> None:
  _, _, trigger_mgr, session_id = await conftest.make_trigger_setup(tmp_path)
  subprocess_mock = _mk_watched_launch_mock(["ALIVE", "DEAD"], ("", 1))
  target = models.RemotePid(
      host="neptune", pid=1234, exit_file="/tmp/charliebot_runs/x/exit", log="/tmp/charliebot_runs/x/log")

  with conftest.patch_trigger_fire(subprocess_mock, sacct_available=None, sleep_mock=conftest.no_sleep) as mock_master:
    trigger = await trigger_mgr.create_trigger(
        session_id, delay_seconds=600, message="remote launcher gone", watch_targets=[target])
    await asyncio.wait_for(trigger_mgr._tasks[trigger.id], timeout=10)

  msg = await conftest.assert_trigger_fired(trigger_mgr, session_id, trigger.id, mock_master, reason="completed")
  assert "finished: neptune:1234: exit file missing, log /tmp/charliebot_runs/x/log" in msg


@pytest.mark.asyncio
async def test_remote_finished_label_both_fields_null_keeps_host_pid(tmp_path: pathlib.Path) -> None:
  _, _, trigger_mgr, session_id = await conftest.make_trigger_setup(tmp_path)
  subprocess_mock = _mk_watched_launch_mock(["ALIVE", "DEAD"], None)

  with conftest.patch_trigger_fire(subprocess_mock, sacct_available=None, sleep_mock=conftest.no_sleep) as mock_master:
    trigger = await trigger_mgr.create_trigger(
        session_id,
        delay_seconds=600,
        message="remote bare",
        watch_targets=[models.RemotePid(host="neptune", pid=1234)],
    )
    await asyncio.wait_for(trigger_mgr._tasks[trigger.id], timeout=10)

  msg = await conftest.assert_trigger_fired(trigger_mgr, session_id, trigger.id, mock_master, reason="completed")
  assert "finished: neptune:1234)" in msg


@pytest.mark.asyncio
async def test_remote_still_alive_at_timeout_carries_the_log(tmp_path: pathlib.Path) -> None:
  _, _, trigger_mgr, session_id = await conftest.make_trigger_setup(tmp_path)
  subprocess_mock = _mk_watched_launch_mock(["ALIVE"], None)
  target = models.RemotePid(
      host="neptune", pid=1234, exit_file="/tmp/charliebot_runs/x/exit", log="/tmp/charliebot_runs/x/log")

  with conftest.patch_trigger_fire(subprocess_mock, sacct_available=None, sleep_mock=conftest.no_sleep) as mock_master:
    trigger = await trigger_mgr.create_trigger(
        session_id, delay_seconds=0, message="remote timeout", watch_targets=[target])
    await asyncio.wait_for(trigger_mgr._tasks[trigger.id], timeout=10)

  msg = await conftest.assert_trigger_fired(trigger_mgr, session_id, trigger.id, mock_master, reason="timeout")
  assert "still alive: neptune:1234, log /tmp/charliebot_runs/x/log" in msg
