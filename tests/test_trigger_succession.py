"""Tests for succession-aware trigger firing and master wake protocol."""

from __future__ import annotations

import asyncio
import pathlib
from unittest import mock

import conftest
import pytest

from src.infra import config, models
from src.infra import event_types as ET
from src.runtime import master_trigger, sessions, triggers
from src.runtime.api.message_utils import build_agent_message_event


def _record_launches(tree) -> list[tuple[str, list[str]]]:
  """Install an executor that records (session id, pending input contents) per launch."""
  launches: list[tuple[str, list[str]]] = []

  async def executor(session_id: str, pending: list[dict], launch_run_id: str | None = None) -> str:
    launches.append((session_id, [e["content"] for e in pending]))
    return "run-1"

  tree.dispatch.executor = executor
  return launches


@pytest.mark.asyncio
async def test_trigger_master_relays_into_the_successor_when_requested_session_eloned(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, mgr, tree = conftest.build_env(tmp_path)
  conftest.bind_deps_managers(monkeypatch, tree, mgr)
  launches = _record_launches(tree)
  parent_id = await conftest.make_parent(mgr)
  child_id = (await mgr.elone_session(parent_id, event_index=0)).id

  await master_trigger.trigger_master(parent_id, "summary", cfg, mgr, ET.CHILD_REPORT)

  # The wake's input sat in the predecessor's log; the successor's own log now holds it.
  assert launches == [(child_id, ["summary"])]
  [relayed] = tree.dispatch.pending_inputs(child_id)
  assert (relayed["type"], relayed["from_session"], relayed["actor"]) == (ET.AGENT_MESSAGE, parent_id, "system")


@pytest.mark.asyncio
async def test_trigger_master_dispatches_the_input_a_task_node_already_holds(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, mgr, tree = conftest.build_env(tmp_path)
  conftest.bind_deps_managers(monkeypatch, tree, mgr)
  launches = _record_launches(tree)
  root = await conftest.create_task(tree, parent=None, request_id="root", name="Root")
  persisted = build_agent_message_event("summon prompt", from_session=root.id, from_session_name="Slack")
  await mgr.persist_and_broadcast(root.id, persisted)

  await master_trigger.trigger_master(
      root.id, "summon prompt", cfg, mgr, ET.AGENT_MESSAGE, user_event_id=persisted["id"])

  assert launches == [(root.id, ["summon prompt"])]  # one input: the wake adds no second copy
  assert [e["id"] for e in tree.dispatch.pending_inputs(root.id)] == [persisted["id"]]


@pytest.mark.asyncio
async def test_firing_trigger_eloned_delivers_into_successor_and_wakes(tmp_path: pathlib.Path) -> None:
  cfg = config.CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = sessions.SessionManager(cfg)
  parent_id = await conftest.make_parent(mgr)
  child_id = (await mgr.elone_session(parent_id, event_index=0)).id
  trigger_mgr = triggers.TriggerManager(cfg, mgr)

  with (
      conftest.patch_trigger_mocks() as mock_master,
      mock.patch(conftest.TRIGGERS_GET_CONFIG_PATCH_TARGET, return_value=cfg),
  ):
    trigger = await trigger_mgr.create_trigger(parent_id, delay_seconds=0, message="wake successor")
    task = trigger_mgr._tasks[trigger.id]
    await asyncio.wait_for(task, timeout=5)

  stored = await trigger_mgr._load_trigger(parent_id, trigger.id)
  assert stored.status == models.TriggerStatus.FIRED

  child_events = mgr.load_chat_events_sync(child_id)
  assert any("wake successor" in ev.get("content", "") for ev in child_events)
  delivered = next(ev for ev in child_events if "wake successor" in ev.get("content", ""))
  assert delivered.get("origin_session_id") == parent_id

  # The trigger record stays in the original session's directory, still FIRED there.
  assert not (mgr._session_dir(child_id) / "triggers" / f"{trigger.id}.json").exists()

  mock_master.assert_awaited_once()
  assert mock_master.await_args.args[0] == parent_id
