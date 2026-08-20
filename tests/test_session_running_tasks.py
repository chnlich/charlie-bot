"""Tests for SessionManager._has_running_tasks window-gated thread scan.

The sidebar listing (GET /api/sessions/) and status poll (GET /api/sessions/status)
both reach _has_running_tasks per active session. It must answer "is a thread
running?" by reading only recently-modified thread metadata: a live thread's
metadata.json is recent by definition, so a stale 'running' on disk is a crashed
orphan, not a live task, and its content should not be read at all.
"""

import json
import os
from datetime import timedelta
from pathlib import Path
from typing import Any

import pytest

from src.core import init as init_module
from src.core import init_worker_recovery as worker_recovery_module
from src.core.config import CharlieBotConfig
from src.core.models import CreateSessionRequest, utc_now
from src.core.sessions import SessionManager


def _write_thread_meta(cfg: CharlieBotConfig, session_id: str, meta: dict) -> Path:
  thread_dir = cfg.sessions_dir / session_id / "threads" / meta["id"]
  thread_dir.mkdir(parents=True, exist_ok=True)
  path = thread_dir / "metadata.json"
  path.write_text(json.dumps(meta), encoding="utf-8")
  return path


@pytest.mark.asyncio
async def test_has_running_tasks_true_for_recent_running_thread(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name="Live"))
  _write_thread_meta(cfg, session.id, {"id": "t1", "status": "running"})

  assert await mgr._has_running_tasks(session.id) is True


@pytest.mark.asyncio
async def test_has_running_tasks_false_for_stale_running_thread_without_reading_it(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name="Stale"))
  stale = _write_thread_meta(cfg, session.id, {"id": "t1", "status": "running"})
  # Age the metadata mtime past the window: an old 'running' is a crashed orphan.
  old_ts = (utc_now() - init_module.RUNNING_SCAN_WINDOW - timedelta(days=1)).timestamp()
  os.utime(stale, (old_ts, old_ts))

  read_paths: list[Path] = []
  real_load = worker_recovery_module.load_json_meta

  def spy(path: Path, log_event: str, **kwargs: Any) -> Any:
    read_paths.append(Path(path))
    return real_load(path, log_event, **kwargs)

  monkeypatch.setattr(worker_recovery_module, "load_json_meta", spy)

  assert await mgr._has_running_tasks(session.id) is False
  # scandir+stat only — the stale metadata content is never read.
  assert stale not in read_paths


@pytest.mark.asyncio
async def test_has_running_tasks_false_when_no_threads(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name="Empty"))

  assert await mgr._has_running_tasks(session.id) is False
