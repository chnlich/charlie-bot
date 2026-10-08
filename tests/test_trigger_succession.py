"""Tests for succession-aware trigger firing and master wake protocol."""

from __future__ import annotations

import pathlib

import conftest
import pytest

from src.infra import event_types as ET
from src.runtime import master_trigger
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
async def test_trigger_master_dispatches_the_input_a_task_node_already_holds(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  _cfg, mgr, tree = conftest.build_env(tmp_path)
  conftest.bind_deps_managers(monkeypatch, tree, mgr)
  launches = _record_launches(tree)
  root = await conftest.create_task(tree, parent=None, request_id="root", name="Root")
  persisted = build_agent_message_event("summon prompt", from_session=root.id, from_session_name="Slack")
  await mgr.events.persist_and_broadcast(root.id, persisted)

  await master_trigger.trigger_master(
      root.id, "summon prompt", mgr, event_type=ET.AGENT_MESSAGE, input_id=persisted["id"])

  assert launches == [(root.id, ["summon prompt"])]  # one input: the wake adds no second copy
  assert [e["id"] for e in tree.dispatch.pending_inputs(root.id)] == [persisted["id"]]
