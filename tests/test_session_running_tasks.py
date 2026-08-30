"""Tests for SessionManager._has_running_tasks window-gated thread scan.

The sidebar listing (GET /api/sessions/) and status poll (GET /api/sessions/status)
both reach _has_running_tasks per active session. It must answer "is a thread
running?" by reading only recently-modified thread metadata: a live thread's
metadata.json is recent by definition, so a stale 'running' on disk is a crashed
orphan, not a live task, and its content should not be read at all.
"""

import os
from datetime import timedelta
from pathlib import Path

import pytest
from conftest import make_home_session, spy_on_load_json_meta, write_thread_meta

from src.core import init as init_module
from src.core.models import utc_now


@pytest.mark.asyncio
async def test_has_running_tasks_true_for_recent_running_thread(tmp_path: Path) -> None:
  cfg, mgr, session = await make_home_session(tmp_path, name="Live")
  write_thread_meta(cfg, session.id, {"id": "t1", "status": "running"})

  assert await mgr._has_running_tasks(session.id) is True


@pytest.mark.asyncio
async def test_has_running_tasks_false_for_stale_running_thread_without_reading_it(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, mgr, session = await make_home_session(tmp_path, name="Stale")
  stale = write_thread_meta(cfg, session.id, {"id": "t1", "status": "running"})
  # Age the metadata mtime past the window: an old 'running' is a crashed orphan.
  old_ts = (utc_now() - init_module.RUNNING_SCAN_WINDOW - timedelta(days=1)).timestamp()
  os.utime(stale, (old_ts, old_ts))

  read_paths = spy_on_load_json_meta(monkeypatch)

  assert await mgr._has_running_tasks(session.id) is False
  # scandir+stat only — the stale metadata content is never read.
  assert stale not in read_paths


@pytest.mark.asyncio
async def test_has_running_tasks_false_when_no_threads(tmp_path: Path) -> None:
  _cfg, mgr, session = await make_home_session(tmp_path, name="Empty")

  assert await mgr._has_running_tasks(session.id) is False
