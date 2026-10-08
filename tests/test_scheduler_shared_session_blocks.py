"""Scheduled rounds must write through the same session blocks the read paths use.

A private events block inside Scheduler keeps its own chat-event cache, so a cron
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

from src.features.cron.config import ScheduledTaskConfig
from src.infra import config
from src.infra import event_types as ET


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
  session_blocks = conftest.build_session_blocks(cfg)
  tree = conftest.build_task_tree(cfg, session_blocks)
  conftest.bind_deps_blocks(monkeypatch, tree, session_blocks)
  # The scheduler reloads the process config on every fire; pin the reload to
  # the synthetic home's in-memory cfg.
  monkeypatch.setattr(conftest.SCHEDULER_LOAD_CONFIG_PATCH_TARGET, lambda: cfg)
  scheduler = conftest.build_scheduler(cfg, session_blocks)
  return cfg, session_blocks, tree, scheduler, monkeypatch


@pytest.mark.asyncio
async def test_scheduled_fire_bookkeeping_writes_the_injected_session_blocks(scheduler_env,) -> None:
  """The fire's durable bookkeeping (last_scheduled_run) and its event land on
  the injected instance, so the read paths' cache sees them."""
  _cfg, session_blocks, tree, scheduler, _monkeypatch = scheduler_env
  meta = await conftest.create_scheduled_node(tree, name="nightly", backend=conftest.OPUS_BACKEND_ID)
  task_cfg = ScheduledTaskConfig(name="nightly", cron="* * * * *", handler="probe", session_id=meta.id)

  with conftest.registered_cron_handler("probe", mock.AsyncMock(return_value="done")):
    await scheduler._execute_task(task_cfg)

  fresh = await session_blocks.store.get_session(meta.id)
  assert fresh is not None
  assert fresh.last_scheduled_run is not None
  events = [e for e in session_blocks.events.load_chat_events_sync(meta.id) if e.get("type") == ET.HANDLER_RESULT]
  assert [event["type"] for event in events] == [ET.HANDLER_RESULT]
  projection = session_blocks.events.get_message_projection(meta.id)
  assert projection is not None
  assert len(projection.history) == 1


@pytest.mark.asyncio
async def test_scheduled_round_events_reach_shared_read_cache(scheduler_env) -> None:
  """After a scheduled round, the read-path cache must still match the file on disk."""
  _cfg, session_blocks, tree, scheduler, _monkeypatch = scheduler_env
  meta = await conftest.create_scheduled_node(tree, name="probe", backend=conftest.OPUS_BACKEND_ID)

  task_cfg = ScheduledTaskConfig(name="probe", cron="* * * * *", handler="probe", session_id=meta.id)
  with conftest.registered_cron_handler("probe", mock.AsyncMock(return_value="done")):
    await scheduler._execute_task(task_cfg)

  disk_lines = _count_event_lines(session_blocks.events.get_chat_events_path(meta.id))
  assert disk_lines > 0, "the round persisted nothing"
  assert session_blocks.events.get_chat_event_count_sync(meta.id) == disk_lines
  types = [event["type"] for event in session_blocks.events.load_chat_events_sync(meta.id)]
  assert ET.HANDLER_RESULT in types
  projection = session_blocks.events.get_message_projection(meta.id)
  assert projection is not None
  assert any(msg.get("role") == "system" for msg in projection.history)
