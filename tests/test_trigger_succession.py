"""Tests for succession-aware trigger firing and master wake protocol."""

from __future__ import annotations

import pathlib

import conftest
import pytest

from src.infra import event_types as ET
from src.runtime import master_trigger
from src.runtime.api import message_utils


@pytest.mark.asyncio
async def test_trigger_master_dispatches_the_input_a_task_node_already_holds(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  _cfg, mgr, tree = conftest.build_env(tmp_path)
  conftest.bind_deps_blocks(monkeypatch, tree, mgr)
  launches = conftest.record_launches(tree)
  root = await conftest.create_task(tree, parent=None, request_id="root", name="Root")
  persisted = message_utils.build_agent_message_event("summon prompt", from_session=root.id, from_session_name="Slack")
  await mgr.events.persist_and_broadcast(root.id, persisted)

  await master_trigger.trigger_master(
      root.id,
      "summon prompt",
      mgr.store,
      mgr.successor,
      mgr.lifecycle,
      event_type=ET.AGENT_MESSAGE,
      input_id=persisted["id"])

  assert launches == [(root.id, ["summon prompt"])]  # one input: the wake adds no second copy
  assert [e["id"] for e in tree.dispatch.pending_inputs(root.id)] == [persisted["id"]]
