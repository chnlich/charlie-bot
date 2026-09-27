"""Unit tests for schedule_trigger optional PID watching."""
from __future__ import annotations

import asyncio
import os
import subprocess
import sys
from pathlib import Path

import pytest
from conftest import (
    assert_trigger_fired,
    patch_trigger_mocks,
)
from conftest import make_trigger_setup as _make_mgr

from src.core.models import LocalPid


def _local(*pids: int) -> list[LocalPid]:
  return [LocalPid(pid=p) for p in pids]


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

  with patch_trigger_mocks() as mock_master:
    trigger = await trigger_mgr.create_trigger(
        session_id,
        delay_seconds=30,
        message="watch gone",
        watch_targets=_local(missing_pid),
    )
    task = trigger_mgr._tasks[trigger.id]
    await asyncio.wait_for(task, timeout=5)

  msg = await assert_trigger_fired(trigger_mgr, session_id, trigger.id, mock_master, reason="completed")
  assert f"finished: {missing_pid} (gone at start)" in msg


@pytest.mark.asyncio
async def test_timeout_before_pid_exit(tmp_path: Path, pidfd_open_available: None) -> None:
  """A window that is already expired (delay 0, the injected stall) fires the
  timeout verdict while the watched process is still alive."""
  _, _, trigger_mgr, session_id = await _make_mgr(tmp_path)

  proc = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"])
  try:
    with patch_trigger_mocks() as mock_master:
      trigger = await trigger_mgr.create_trigger(
          session_id,
          delay_seconds=0,
          message="watch timeout",
          watch_targets=_local(proc.pid),
      )
      task = trigger_mgr._tasks[trigger.id]
      await asyncio.wait_for(task, timeout=10)

    msg = await assert_trigger_fired(trigger_mgr, session_id, trigger.id, mock_master, reason="timeout")
    assert f"still alive: {proc.pid}" in msg
  finally:
    proc.kill()
    proc.wait(timeout=5)
