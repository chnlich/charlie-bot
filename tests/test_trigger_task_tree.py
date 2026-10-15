"""Delayed-trigger delivery to task-tree nodes: the durable dispatcher route.

A v2 node takes the trigger through the shared admission path — the trigger's
own id is the input's stable identity — so a crash after the durable admission
but before the FIRED stamp replays into the SAME input instead of duplicating
the task input or its process. Closed nodes keep late history without
restoring; established aliases resolve without changing task ownership; and
the node's close fact remains the dormancy authority.
"""

from __future__ import annotations

import datetime
import pathlib
from unittest import mock

import conftest
import pytest

from src.infra import event_types as ET
from src.infra import models
from src.runtime import triggers
from tests import test_task_execution


def build_env(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch):
  cfg, session_blocks, tree = test_task_execution.build_spawning_env(
      tmp_path,
      monkeypatch,
      options=[conftest.backend_option(id="fake", label="Fake", type="codex", model="fake-model")])
  tree.dispatch.executor = test_task_execution._adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  return cfg, session_blocks, tree


def _trigger(session_id: str, trigger_id: str = "trigger-v2-1") -> models.PendingTrigger:
  return models.PendingTrigger(
      id=trigger_id,
      session_id=session_id,
      fire_at=datetime.datetime.now(datetime.UTC),
      message="Check PID 12345",
  )


@pytest.mark.asyncio
async def test_trigger_admits_one_durable_input_to_task_tree_node(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  conftest.bind_deps_blocks(monkeypatch, tree, session_blocks)
  builds = conftest.install_scripted_backends(
      monkeypatch, [test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("awake")])],
      conftest.BUILD_BACKEND_PATCH_TARGET)
  manager = await tree.create_task(
      request_id="pm",
      task_parent_id=None,
      profile="manager",
      task=models.TaskSpec(goal="pm"),
      name="PM",
      backend=None,
      caller="operator")
  trigger_mgr = triggers.TriggerManager(cfg, tree)
  trigger = _trigger(manager.id)
  await trigger_mgr._save_trigger(trigger)

  with (mock.patch(conftest.BROADCAST_PATCH_TARGET, new=mock.AsyncMock()),):
    await trigger_mgr._wait_and_fire(trigger)

  # The trigger lands as ONE durable scheduled input with the trigger's own id,
  # and the manager turn launches through the shared dispatcher.
  events = tree.events.load_events(manager.id)
  trigger_events = [e for e in events if e.get("type") == ET.SCHEDULED_TRIGGER]
  assert len(trigger_events) == 1
  assert trigger_events[0].get("id") == trigger.id
  assert "Check PID 12345" in str(trigger_events[0].get("content"))
  fresh = await trigger_mgr._load_trigger(trigger.session_id, trigger.id)
  assert fresh is not None and fresh.status == models.TriggerStatus.FIRED
  turn_id = tree.runs.list_run_records_sync(manager.id)[0].id
  await test_task_execution.wait_for_terminal_run(tree, manager.id, turn_id)
  assert len(builds) == 1
