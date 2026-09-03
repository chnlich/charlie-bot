"""Scheduled rounds must write through the same SessionManager the read paths use.

A private SessionManager inside Scheduler keeps its own chat-event cache, so a cron
round would land on disk while /bootstrap, /view and WS catchup — which all read the
process-wide instance's cache — keep serving the pre-cron history.
"""

from collections.abc import Coroutine
from pathlib import Path
from typing import Any
from unittest.mock import AsyncMock

import pytest
from conftest import (
    OPUS_BACKEND_ID,
    OPUS_BACKEND_OPTION,
    SCHEDULER_CREATE_LOGGED_TASK_PATCH_TARGET,
    SCHEDULER_GET_CONFIG_PATCH_TARGET,
    SCHEDULER_RESOLVE_SUBAGENT_BACKEND_MODEL_PATCH_TARGET,
    SCHEDULER_SPAWN_WORKER_PATCH_TARGET,
    SCHEDULER_THREAD_MANAGER_PATCH_TARGET,
    FakeThreadManager,
    close_create_logged_task,
)

from src.core import event_types as ET
from src.core.config import CharlieBotConfig, ScheduledTaskConfig
from src.core.models import CreateSessionRequest
from src.core.scheduler import TASK_HANDLERS, Scheduler
from src.core.sessions import SessionManager


def _build_cfg(tmp_path: Path) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=tmp_path / "charliebot-home",
      worktree_dir=str(tmp_path / "worktrees"),
      backend_options=[
          OPUS_BACKEND_OPTION,
      ],
  )


def _count_event_lines(path: Path) -> int:
  if not path.exists():
    return 0
  return len([line for line in path.read_text(encoding="utf-8").splitlines() if line.strip()])


@pytest.mark.asyncio
async def test_scheduled_prompt_task_hands_injected_session_manager_to_worker(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  """The worker (and through it the master wake) must write on the injected instance."""
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  await session_mgr.create_session(
      CreateSessionRequest(name="Scheduled: nightly", scheduled_task="nightly"),
      backend=OPUS_BACKEND_ID,
  )
  scheduler = Scheduler(cfg, session_mgr)
  task_cfg = ScheduledTaskConfig(name="nightly", cron="* * * * *", prompt="nightly prompt")

  captured: dict[str, Any] = {}

  def fake_spawn_worker(**kwargs: Any) -> Coroutine[Any, Any, None]:
    captured.update(kwargs)
    return _noop()

  async def _noop() -> None:
    return None

  monkeypatch.setattr(SCHEDULER_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  monkeypatch.setattr(SCHEDULER_THREAD_MANAGER_PATCH_TARGET, lambda _cfg: FakeThreadManager())
  monkeypatch.setattr(
      SCHEDULER_RESOLVE_SUBAGENT_BACKEND_MODEL_PATCH_TARGET,
      AsyncMock(return_value=(OPUS_BACKEND_ID, OPUS_BACKEND_OPTION.model)),
  )
  monkeypatch.setattr(SCHEDULER_SPAWN_WORKER_PATCH_TARGET, fake_spawn_worker)
  monkeypatch.setattr(SCHEDULER_CREATE_LOGGED_TASK_PATCH_TARGET, close_create_logged_task)

  await scheduler._execute_task(task_cfg)

  assert captured, "spawn_worker was never called"
  assert captured["session_mgr"] is session_mgr


@pytest.mark.asyncio
async def test_scheduled_round_events_reach_shared_read_cache(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  """After a scheduled round, the read-path cache must still match the file on disk."""
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = await session_mgr.create_session(
      CreateSessionRequest(name="Scheduled: probe", scheduled_task="probe"),
      backend=OPUS_BACKEND_ID,
  )
  # Warm the cache the way an HTTP history read does, before the round fires.
  assert not session_mgr.load_chat_events_sync(meta.id)

  scheduler = Scheduler(cfg, session_mgr)
  monkeypatch.setattr(SCHEDULER_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  monkeypatch.setitem(TASK_HANDLERS, "probe", AsyncMock(return_value="done"))
  task_cfg = ScheduledTaskConfig(name="probe", cron="* * * * *", handler="probe")

  await scheduler._execute_task(task_cfg)

  disk_lines = _count_event_lines(session_mgr.get_chat_events_path(meta.id))
  assert disk_lines > 0, "the round persisted nothing"
  assert session_mgr.get_chat_event_count_sync(meta.id) == disk_lines
  assert [event["type"] for event in session_mgr.load_chat_events_sync(meta.id)] == [ET.HANDLER_RESULT]
  projection = session_mgr.get_message_projection(meta.id)
  assert projection is not None
  assert len(projection.history) == 1
