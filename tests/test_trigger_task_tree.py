"""Delayed-trigger delivery to task-tree nodes: the durable dispatcher route.

A v2 node takes the trigger through the shared admission path — the trigger's
own id is the input's stable identity — so a crash after the durable admission
but before the FIRED stamp replays into the SAME input instead of duplicating
the task input or its process. Closed nodes keep late history without
reopening; established aliases resolve without changing task ownership; and
the legacy route keeps serving v1 sessions unchanged.
"""

from __future__ import annotations

from datetime import UTC, datetime
from pathlib import Path
from unittest.mock import AsyncMock, patch

import pytest
from conftest import (
    BROADCAST_PATCH_TARGET,
    BUILD_BACKEND_PATCH_TARGET,
    TRIGGER_MASTER_PATCH_TARGET,
    TRIGGERS_GET_CONFIG_PATCH_TARGET,
    bind_deps_managers,
    patch_instructions_content,
)

from src.core import event_types as ET
from src.core.models import PendingTrigger, TaskSpec, TriggerStatus
from src.core.sessions import SessionManager
from src.core.task_sessions import TaskTreeManager
from src.core.triggers import TriggerManager
from tests.test_task_execution import (
    SpawningScriptedBackend,
    _adapter_with_silent_broadcast,
    install_backends,
    result_event,
)


def build_env(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
  from conftest import backend_option

  import src.core.config as core_config
  from src.core.config import CharlieBotConfig
  home = tmp_path / "charliebot-home"
  cfg = CharlieBotConfig(
      charliebot_home=home,
      backends={"options": [backend_option(id="fake", label="Fake", type="codex", model="fake-model")]},
      paths={"worktree_dir": str(home / "worktrees")})
  core_config._credentials_cache.seed(
      core_config.Credentials(path=home / "credentials.yaml", sections={"charliebot": {
          "access_key": "trigger-key"
      }}))
  monkeypatch.setenv("CHARLIEBOT_HOME", str(home))
  session_mgr = SessionManager(cfg)
  tree = TaskTreeManager(cfg, session_mgr)
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
  return cfg, session_mgr, tree


def _trigger(session_id: str, trigger_id: str = "trigger-v2-1") -> PendingTrigger:
  return PendingTrigger(
      id=trigger_id,
      session_id=session_id,
      fire_at=datetime.now(UTC),
      message="Check PID 12345",
  )


@pytest.mark.asyncio
async def test_trigger_admits_one_durable_input_to_task_tree_node(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
  bind_deps_managers(monkeypatch, tree, session_mgr)
  builds = install_backends(monkeypatch, [SpawningScriptedBackend([result_event("awake")])], BUILD_BACKEND_PATCH_TARGET)
  patch_instructions_content(monkeypatch)
  manager = await tree.create_task(
      request_id="pm",
      task_parent_id=None,
      profile="manager",
      task=TaskSpec(goal="pm"),
      name="PM",
      backend=None,
      caller="operator")
  trigger_mgr = TriggerManager(cfg, session_mgr)
  trigger = _trigger(manager.id)
  await trigger_mgr._save_trigger(trigger)

  with (
      patch(BROADCAST_PATCH_TARGET, new=AsyncMock()),
      patch(TRIGGER_MASTER_PATCH_TARGET, new=AsyncMock()) as mock_trigger_master,
      patch(TRIGGERS_GET_CONFIG_PATCH_TARGET, return_value=cfg),
  ):
    await trigger_mgr._wait_and_fire(trigger)

  # The trigger lands as ONE durable scheduled input with the trigger's own id,
  # and the manager turn launches through the shared dispatcher.
  events = tree.events.load_events(manager.id)
  triggers = [e for e in events if e.get("type") == ET.SCHEDULED_TRIGGER]
  assert len(triggers) == 1
  assert triggers[0].get("id") == trigger.id
  assert "Check PID 12345" in str(triggers[0].get("content"))
  fresh = await trigger_mgr._load_trigger(trigger.session_id, trigger.id)
  assert fresh is not None and fresh.status == TriggerStatus.FIRED
  from tests.test_task_execution import wait_for_terminal_run
  turn_id = tree.runs.list_run_records_sync(manager.id)[0].id
  await wait_for_terminal_run(tree, manager.id, turn_id)
  assert len(builds) == 1
  # The legacy wake path never runs for a v2 node (no double admission).
  mock_trigger_master.assert_not_awaited()
