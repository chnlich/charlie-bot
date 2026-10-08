"""Scheduled rounds must write through the same SessionManager the read paths use.

A private SessionManager inside Scheduler keeps its own chat-event cache, so a cron
round would land on disk while /bootstrap and WS catchup — which all read the
process-wide instance's cache — keep serving the pre-cron history. The scheduler's
bookkeeping goes through the task-tree owner (update_slot_fields) and its events
through persist_and_broadcast, so both singletons must be the injected instances.
"""

from __future__ import annotations

import pathlib
from unittest import mock

import conftest
import pytest

from src.features.cron.scheduler import Scheduler
from src.infra import config
from src.infra import event_types as ET
from src.runtime import sessions, task_sessions


def _count_event_lines(path: pathlib.Path) -> int:
  if not path.exists():
    return 0
  return len([line for line in path.read_text(encoding="utf-8").splitlines() if line.strip()])


@pytest.fixture()
def scheduler_env(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch):
  """One synthetic home with the scheduler's deps singletons wired to it."""
  import src.infra.config as core_config
  home = tmp_path / "charliebot-home"
  cfg = config.CharlieBotConfig(
      charliebot_home=home,
      backends={"options": [conftest.OPUS_BACKEND_OPTION]},
      paths={"worktree_dir": str(home / "worktrees")})
  core_config._credentials_cache.seed(
      core_config.Credentials(path=home / "credentials.yaml", sections={"charliebot": {
          "access_key": "shared-key"
      }}))
  monkeypatch.setenv("CHARLIEBOT_HOME", str(home))
  session_mgr = sessions.SessionManager(cfg)
  tree = task_sessions.TaskTreeManager(cfg, session_mgr)
  conftest.bind_deps_managers(monkeypatch, tree, session_mgr)
  # The scheduler reloads the process config on every fire; pin the reload to
  # the synthetic home's in-memory cfg.
  monkeypatch.setattr(conftest.SCHEDULER_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  scheduler = Scheduler(cfg, session_mgr)
  return cfg, session_mgr, tree, scheduler, monkeypatch


@pytest.mark.asyncio
async def test_scheduled_fire_bookkeeping_writes_the_injected_session_manager(scheduler_env,) -> None:
  """The fire's durable bookkeeping (last_scheduled_run) and its event land on
  the injected instance, so the read paths' cache sees them."""
  _cfg, session_mgr, tree, scheduler, _monkeypatch = scheduler_env
  meta = await conftest.create_scheduled_node(tree, name="nightly", backend=conftest.OPUS_BACKEND_ID)
  task_cfg = config.ScheduledTaskConfig(name="nightly", cron="* * * * *", handler="probe", session_id=meta.id)

  with conftest.registered_cron_handler("probe", mock.AsyncMock(return_value="done")):
    await scheduler._execute_task(task_cfg)

  fresh = await session_mgr.get_session(meta.id)
  assert fresh is not None
  assert fresh.last_scheduled_run is not None
  events = [e for e in session_mgr.load_chat_events_sync(meta.id) if e.get("type") == ET.HANDLER_RESULT]
  assert [event["type"] for event in events] == [ET.HANDLER_RESULT]
  projection = session_mgr.get_message_projection(meta.id)
  assert projection is not None
  assert len(projection.history) == 1


@pytest.mark.asyncio
async def test_scheduled_round_events_reach_shared_read_cache(scheduler_env) -> None:
  """After a scheduled round, the read-path cache must still match the file on disk."""
  _cfg, session_mgr, tree, scheduler, _monkeypatch = scheduler_env
  meta = await conftest.create_scheduled_node(tree, name="probe", backend=conftest.OPUS_BACKEND_ID)

  task_cfg = config.ScheduledTaskConfig(name="probe", cron="* * * * *", handler="probe", session_id=meta.id)
  with conftest.registered_cron_handler("probe", mock.AsyncMock(return_value="done")):
    await scheduler._execute_task(task_cfg)

  disk_lines = _count_event_lines(session_mgr.get_chat_events_path(meta.id))
  assert disk_lines > 0, "the round persisted nothing"
  assert session_mgr.get_chat_event_count_sync(meta.id) == disk_lines
  types = [event["type"] for event in session_mgr.load_chat_events_sync(meta.id)]
  assert ET.HANDLER_RESULT in types
  projection = session_mgr.get_message_projection(meta.id)
  assert projection is not None
  assert any(msg.get("role") == "system" for msg in projection.history)
