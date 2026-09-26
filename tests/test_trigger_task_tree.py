"""Delayed-trigger delivery to task-tree nodes: the durable dispatcher route.

A v2 node takes the trigger through the shared admission path — the trigger's
own id is the input's stable identity — so a crash after the durable admission
but before the FIRED stamp replays into the SAME input instead of duplicating
the task input or its process. Closed nodes keep late history without
reopening; paused nodes retain the admitted input for later dispatch;
established aliases resolve without changing task ownership; and the legacy
route keeps serving v1 sessions unchanged.
"""

from __future__ import annotations

import json
from datetime import UTC, datetime
from pathlib import Path
from unittest.mock import AsyncMock, patch

import pytest
from conftest import (
  BROADCAST_PATCH_TARGET,
  BUILD_BACKEND_PATCH_TARGET,
  TRIGGER_MASTER_PATCH_TARGET,
  TRIGGERS_GET_CONFIG_PATCH_TARGET,
  make_home_config,
  patch_instructions_content,
)

from src.core import event_types as ET
from src.core.models import CreateSessionRequest, PendingTrigger, TaskSpec, TriggerStatus
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
  core_config._credentials_cache.seed(core_config.Credentials(
      path=home / "credentials.yaml", sections={"charliebot": {"access_key": "trigger-key"}}))
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
  from src.api import deps
  monkeypatch.setattr(deps, "_task_manager", tree)
  monkeypatch.setattr(deps, "_session_manager", session_mgr)
  builds = install_backends(monkeypatch, [SpawningScriptedBackend([result_event("awake")])],
                            BUILD_BACKEND_PATCH_TARGET)
  patch_instructions_content(monkeypatch)
  manager = await tree.create_task(
      request_id="pm", task_parent_id=None, profile="manager",
      task=TaskSpec(goal="pm"), name="PM", backend=None, caller="operator")
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


@pytest.mark.asyncio
async def test_trigger_refire_after_crash_does_not_duplicate_input(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Crash after the durable admission but before FIRED: the recovered trigger
  re-fires into the SAME input and stamps FIRED once."""
  cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
  from src.api import deps
  monkeypatch.setattr(deps, "_task_manager", tree)
  monkeypatch.setattr(deps, "_session_manager", session_mgr)
  manager = await tree.create_task(
      request_id="pm", task_parent_id=None, profile="manager",
      task=TaskSpec(goal="pm"), name="PM", backend=None, caller="operator")
  trigger = _trigger(manager.id)
  trigger_mgr = TriggerManager(cfg, session_mgr)
  await trigger_mgr._save_trigger(trigger)

  with (
      patch(BROADCAST_PATCH_TARGET, new=AsyncMock()),
      patch(TRIGGERS_GET_CONFIG_PATCH_TARGET, return_value=cfg),
      patch.object(TriggerManager, "_fire_task_tree", AsyncMock(side_effect=RuntimeError("crashed"))),
      pytest.raises(RuntimeError, match="crashed"),
  ):
    await trigger_mgr._wait_and_fire(trigger)
  # The trigger stays pending (the crash window): no FIRED stamp.
  fresh = await trigger_mgr._load_trigger(trigger.session_id, trigger.id)
  assert fresh is not None and fresh.status == TriggerStatus.PENDING

  # Recovery re-fires it: the same input id dedups and FIRED lands.
  builds = install_backends(monkeypatch, [SpawningScriptedBackend([result_event("awake")])],
                            BUILD_BACKEND_PATCH_TARGET)
  patch_instructions_content(monkeypatch)
  with (
      patch(BROADCAST_PATCH_TARGET, new=AsyncMock()),
      patch(TRIGGER_MASTER_PATCH_TARGET, new=AsyncMock()) as mock_trigger_master,
      patch(TRIGGERS_GET_CONFIG_PATCH_TARGET, return_value=cfg),
  ):
    await trigger_mgr._wait_and_fire(trigger)
  events = tree.events.load_events(manager.id)
  assert len([e for e in events if e.get("type") == ET.SCHEDULED_TRIGGER]) == 1
  fresh = await trigger_mgr._load_trigger(trigger.session_id, trigger.id)
  assert fresh is not None and fresh.status == TriggerStatus.FIRED
  mock_trigger_master.assert_not_awaited()
  from tests.test_task_execution import wait_for_terminal_run
  turn_id = tree.runs.list_run_records_sync(manager.id)[0].id
  await wait_for_terminal_run(tree, manager.id, turn_id)
  assert len(builds) == 1


@pytest.mark.asyncio
async def test_trigger_on_closed_node_keeps_history_without_reopening(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
  from src.api import deps
  monkeypatch.setattr(deps, "_task_manager", tree)
  monkeypatch.setattr(deps, "_session_manager", session_mgr)
  from src.core.run_token import CallerIdentity
  from src.core.task_completion import CompletionEvidence
  manager = await tree.create_task(
      request_id="pm", task_parent_id=None, profile="manager",
      task=TaskSpec(goal="pm"), name="PM", backend=None, caller="operator")
  close_run = "run-close-" + manager.id[:8]
  from src.core.control_events import stable_run_id
  close_run = stable_run_id(manager.id, "close:evidence")
  from src.core.models import RunRecord
  await tree.runs.register_run(RunRecord(id=close_run, session_id=manager.id, kind="manager_turn"))
  await tree.runs.record_launch(manager.id, close_run, pid=424001, pid_start="ps-1")
  await tree.dispatch.finish_run(manager.id, close_run, outcome="success")
  await tree.completion.complete_task(
      manager.id, request_id="close-1", caller=CallerIdentity(kind="operator"),
      evidence=CompletionEvidence(summary="done", run_ids=[close_run], result_refs=[f"run:{close_run}"]))
  assert tree.task_state(manager.id) == "completed"

  builds = install_backends(monkeypatch, [SpawningScriptedBackend([result_event("x")])],
                            BUILD_BACKEND_PATCH_TARGET)
  trigger_mgr = TriggerManager(cfg, session_mgr)
  trigger = _trigger(manager.id)
  await trigger_mgr._save_trigger(trigger)
  with (
      patch(BROADCAST_PATCH_TARGET, new=AsyncMock()),
      patch(TRIGGER_MASTER_PATCH_TARGET, new=AsyncMock()) as mock_trigger_master,
      patch(TRIGGERS_GET_CONFIG_PATCH_TARGET, return_value=cfg),
  ):
    await trigger_mgr._wait_and_fire(trigger)

  # The late trigger's history is kept on the closed node; nothing reopens and
  # no headless turn starts (the closed decision retains the input).
  events = tree.events.load_events(manager.id)
  assert len([e for e in events if e.get("type") == ET.SCHEDULED_TRIGGER]) == 1
  assert tree.task_state(manager.id) == "completed"
  assert builds == []
  mock_trigger_master.assert_not_awaited()
  fresh = await trigger_mgr._load_trigger(trigger.session_id, trigger.id)
  assert fresh is not None and fresh.status == TriggerStatus.FIRED


@pytest.mark.asyncio
async def test_trigger_on_paused_node_retains_input_for_later_dispatch(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
  from src.api import deps
  monkeypatch.setattr(deps, "_task_manager", tree)
  monkeypatch.setattr(deps, "_session_manager", session_mgr)
  from src.core.models import PatchSessionTaskRequest
  from src.core.run_token import CallerIdentity
  manager = await tree.create_task(
      request_id="pm", task_parent_id=None, profile="manager",
      task=TaskSpec(goal="pm"), name="PM", backend=None, caller="operator")
  await tree.patch_task(
      manager.id, PatchSessionTaskRequest(automation_paused=True),
      caller=CallerIdentity(kind="operator"))

  builds = install_backends(monkeypatch, [SpawningScriptedBackend([result_event("x")])],
                            BUILD_BACKEND_PATCH_TARGET)
  trigger_mgr = TriggerManager(cfg, session_mgr)
  trigger = _trigger(manager.id)
  await trigger_mgr._save_trigger(trigger)
  with (
      patch(BROADCAST_PATCH_TARGET, new=AsyncMock()),
      patch(TRIGGERS_GET_CONFIG_PATCH_TARGET, return_value=cfg),
  ):
    await trigger_mgr._wait_and_fire(trigger)

  # The input is admitted (durable) and retained for later dispatch; nothing
  # launches while paused, and the FIRED stamp stands.
  events = tree.events.load_events(manager.id)
  assert len([e for e in events if e.get("type") == ET.SCHEDULED_TRIGGER]) == 1
  assert builds == []
  fresh = await trigger_mgr._load_trigger(trigger.session_id, trigger.id)
  assert fresh is not None and fresh.status == TriggerStatus.FIRED
  # Resuming dispatches the retained input through the same dispatcher.
  await tree.patch_task(
      manager.id, PatchSessionTaskRequest(automation_paused=False),
      caller=CallerIdentity(kind="operator"))
  decision = await tree.dispatch.dispatch_pending(manager.id)
  assert decision["launch"] is True


@pytest.mark.asyncio
async def test_trigger_resolves_established_alias_to_the_same_task(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
  from src.api import deps
  monkeypatch.setattr(deps, "_task_manager", tree)
  monkeypatch.setattr(deps, "_session_manager", session_mgr)
  manager = await tree.create_task(
      request_id="pm", task_parent_id=None, profile="manager",
      task=TaskSpec(goal="pm"), name="PM", backend=None, caller="operator")
  old_id = "11111111-2222-3333-4444-555555555555"
  tree.aliases.path.parent.mkdir(parents=True, exist_ok=True)
  tree.aliases.path.write_text(json.dumps({
      "old_session_ids": {old_id: manager.id},
      "old_threads": {},
  }, ensure_ascii=False, indent=2, sort_keys=True), encoding="utf-8")

  builds = install_backends(monkeypatch, [SpawningScriptedBackend([result_event("awake")])],
                            BUILD_BACKEND_PATCH_TARGET)
  trigger_mgr = TriggerManager(cfg, session_mgr)
  trigger = _trigger(old_id, trigger_id="trigger-alias-1")
  await trigger_mgr._save_trigger(trigger)
  with (
      patch(BROADCAST_PATCH_TARGET, new=AsyncMock()),
      patch(TRIGGERS_GET_CONFIG_PATCH_TARGET, return_value=cfg),
  ):
    await trigger_mgr._wait_and_fire(trigger)

  # The alias resolved to the canonical node: the input and its process live
  # there, and the old id's ownership never changed.
  events = tree.events.load_events(manager.id)
  assert len([e for e in events if e.get("type") == ET.SCHEDULED_TRIGGER]) == 1
  from tests.test_task_execution import wait_for_terminal_run
  turn_id = tree.runs.list_run_records_sync(manager.id)[0].id
  await wait_for_terminal_run(tree, manager.id, turn_id)
  assert len(builds) == 1


@pytest.mark.asyncio
async def test_trigger_to_legacy_session_keeps_the_legacy_route(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A v1 session keeps the existing append+wake behavior unchanged."""
  cfg = make_home_config(tmp_path)
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="Legacy"))
  trigger_mgr = TriggerManager(cfg, session_mgr)
  trigger = _trigger(session.id, trigger_id="trigger-v1-1")
  await trigger_mgr._save_trigger(trigger)

  with (
      patch(BROADCAST_PATCH_TARGET, new=AsyncMock()),
      patch(TRIGGER_MASTER_PATCH_TARGET, new=AsyncMock()) as mock_trigger_master,
      patch(TRIGGERS_GET_CONFIG_PATCH_TARGET, return_value=cfg),
  ):
    await trigger_mgr._wait_and_fire(trigger)

  events = session_mgr.load_chat_events_sync(session.id)
  assert events[0]["type"] == ET.SCHEDULED_TRIGGER
  mock_trigger_master.assert_awaited_once()
