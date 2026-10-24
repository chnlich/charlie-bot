"""Unit tests for schedule_trigger optional PID watching."""
from __future__ import annotations

import asyncio
import json
import os
import pathlib
import subprocess
import sys

import conftest
import pytest

from src.infra import models


def _local(*pids: int) -> list[models.LocalPid]:
  return [models.LocalPid(pid=p) for p in pids]


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
async def test_pid_gone_immediate_fire(tmp_path: pathlib.Path, pidfd_open_available: None) -> None:
  _, _, trigger_mgr, session_id = await conftest.make_trigger_setup(tmp_path)
  missing_pid = _find_unused_pid()

  with conftest.patch_trigger_mocks() as mock_master:
    trigger = await trigger_mgr.create_trigger(
        session_id,
        delay_seconds=30,
        message="watch gone",
        watch_targets=_local(missing_pid),
    )
    task = trigger_mgr._tasks[trigger.id]
    await asyncio.wait_for(task, timeout=5)

  msg = await conftest.assert_trigger_fired(trigger_mgr, session_id, trigger.id, mock_master, reason="completed")
  assert f"finished: {missing_pid} (gone at start)" in msg


@pytest.mark.asyncio
async def test_timeout_before_pid_exit(tmp_path: pathlib.Path, pidfd_open_available: None) -> None:
  """A window that is already expired (delay 0, the injected stall) fires the
  timeout verdict while the watched process is still alive."""
  _, _, trigger_mgr, session_id = await conftest.make_trigger_setup(tmp_path)

  proc = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"])
  try:
    with conftest.patch_trigger_mocks() as mock_master:
      trigger = await trigger_mgr.create_trigger(
          session_id,
          delay_seconds=0,
          message="watch timeout",
          watch_targets=_local(proc.pid),
      )
      task = trigger_mgr._tasks[trigger.id]
      await asyncio.wait_for(task, timeout=10)

    msg = await conftest.assert_trigger_fired(trigger_mgr, session_id, trigger.id, mock_master, reason="timeout")
    assert f"still alive: {proc.pid}" in msg
  finally:
    proc.kill()
    proc.wait(timeout=5)


def _local_launch(pid: int, exit_file: str | None, log: str | None) -> list[models.LocalPid]:
  return [models.LocalPid(pid=pid, exit_file=exit_file, log=log)]


@pytest.mark.asyncio
async def test_finished_label_reads_the_exit_file(tmp_path: pathlib.Path, pidfd_open_available: None) -> None:
  """A launcher-started target that exits mid-wait labels its exit code from the exit file."""
  _, _, trigger_mgr, session_id = await conftest.make_trigger_setup(tmp_path)
  exit_file = tmp_path / "exit"
  exit_file.write_text("0\n")
  log_path = tmp_path / "log"

  proc = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(0.3)"])
  try:
    with conftest.patch_trigger_mocks() as mock_master:
      trigger = await trigger_mgr.create_trigger(
          session_id,
          delay_seconds=30,
          message="watch launcher",
          watch_targets=_local_launch(proc.pid, str(exit_file), str(log_path)),
      )
      await asyncio.wait_for(trigger_mgr._tasks[trigger.id], timeout=10)

    msg = await conftest.assert_trigger_fired(trigger_mgr, session_id, trigger.id, mock_master, reason="completed")
    assert f"finished: {proc.pid}: exit 0, log {log_path}" in msg
  finally:
    proc.kill()
    proc.wait(timeout=5)


@pytest.mark.asyncio
async def test_finished_label_exit_file_missing_states_no_code(
    tmp_path: pathlib.Path, pidfd_open_available: None) -> None:
  """The exit file absent at fire time: the label says so instead of guessing a code."""
  _, _, trigger_mgr, session_id = await conftest.make_trigger_setup(tmp_path)
  missing_pid = _find_unused_pid()
  missing_exit = tmp_path / "no-such-exit"
  log_path = tmp_path / "log"

  with conftest.patch_trigger_mocks() as mock_master:
    trigger = await trigger_mgr.create_trigger(
        session_id,
        delay_seconds=30,
        message="watch missing exit",
        watch_targets=_local_launch(missing_pid, str(missing_exit), str(log_path)),
    )
    await asyncio.wait_for(trigger_mgr._tasks[trigger.id], timeout=5)

  msg = await conftest.assert_trigger_fired(trigger_mgr, session_id, trigger.id, mock_master, reason="completed")
  assert f"finished: {missing_pid}: exit file missing, log {log_path}" in msg


@pytest.mark.asyncio
async def test_finished_label_both_fields_null_keeps_the_bare_pid(
    tmp_path: pathlib.Path, pidfd_open_available: None) -> None:
  """A target with neither field keeps the bare pid label."""
  _, _, trigger_mgr, session_id = await conftest.make_trigger_setup(tmp_path)
  missing_pid = _find_unused_pid()

  with conftest.patch_trigger_mocks() as mock_master:
    trigger = await trigger_mgr.create_trigger(
        session_id,
        delay_seconds=30,
        message="watch bare",
        watch_targets=_local_launch(missing_pid, None, None),
    )
    await asyncio.wait_for(trigger_mgr._tasks[trigger.id], timeout=5)

  msg = await conftest.assert_trigger_fired(trigger_mgr, session_id, trigger.id, mock_master, reason="completed")
  assert f"finished: {missing_pid} (gone at start)" in msg


@pytest.mark.asyncio
async def test_still_alive_at_timeout_carries_the_log(tmp_path: pathlib.Path, pidfd_open_available: None) -> None:
  """A target still alive at --max-wait reports that fact plus its log path."""
  _, _, trigger_mgr, session_id = await conftest.make_trigger_setup(tmp_path)
  log_path = tmp_path / "log"

  proc = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"])
  try:
    with conftest.patch_trigger_mocks() as mock_master:
      trigger = await trigger_mgr.create_trigger(
          session_id,
          delay_seconds=0,
          message="watch timeout",
          watch_targets=_local_launch(proc.pid, str(tmp_path / "exit"), str(log_path)),
      )
      await asyncio.wait_for(trigger_mgr._tasks[trigger.id], timeout=10)

    msg = await conftest.assert_trigger_fired(trigger_mgr, session_id, trigger.id, mock_master, reason="timeout")
    assert f"still alive: {proc.pid}, log {log_path}" in msg
  finally:
    proc.kill()
    proc.wait(timeout=5)


@pytest.mark.asyncio
async def test_trigger_file_without_exit_fields_still_loads(tmp_path: pathlib.Path) -> None:
  """A trigger file written before exit_file/log existed loads with both None."""
  cfg, _, trigger_mgr, session_id = await conftest.make_trigger_setup(tmp_path)
  old = {
      "id": "old-1",
      "session_id": session_id,
      "fire_at": "2030-01-01T00:00:00+00:00",
      "message": "old watch",
      "created_at": "2026-10-08T00:00:00+00:00",
      "status": "pending",
      "fired_at": None,
      "watch_targets": [{
          "kind": "local_pid",
          "pid": 1234
      }, {
          "kind": "remote_pid",
          "host": "neptune",
          "pid": 5678
      }],
      "fire_reason": None,
  }
  triggers_dir = cfg.sessions_dir / session_id / "triggers"
  triggers_dir.mkdir(parents=True, exist_ok=True)
  (triggers_dir / "old-1.json").write_text(json.dumps(old), encoding="utf-8")

  loaded = await trigger_mgr._load_trigger(session_id, "old-1")
  local, remote = loaded.watch_targets
  assert isinstance(local, models.LocalPid) and local.exit_file is None and local.log is None
  assert isinstance(remote, models.RemotePid) and remote.exit_file is None and remote.log is None
