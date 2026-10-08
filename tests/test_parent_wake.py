"""The one parent-wake entry after a report delivery (wake_parent).

A task-tree parent's next serialized turn dispatches from its durable inputs
and never consults the caller — not even the parent's own turn closing its
child. The report event stays the durable record either way.
"""

from __future__ import annotations

import pathlib
from unittest import mock

import conftest
import pytest

from src.infra import event_types as ET
from src.infra import models

_MASTER_TRIGGER_PATCH_TARGET = "src.runtime.master_trigger.trigger_master"


def child_report(child_session_id: str, *, outcome: str, summary: str, event_id: str) -> dict:
  return {
      "id": event_id,
      "type": ET.CHILD_REPORT,
      "timestamp": models.utc_now_iso(),
      "actor": "system",
      "source_session_id": child_session_id,
      "child_session_id": child_session_id,
      "child_event_id": "close-1",
      "outcome": outcome,
      "summary": summary,
      "result_refs": [],
  }


def _failed_work_run(session_id: str, run_id: str) -> models.RunRecord:
  return models.RunRecord(id=run_id, session_id=session_id, kind="work", backend="fake", model="fake-model")


@pytest.mark.asyncio
async def test_node_parent_dispatches_its_pending_inputs(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  _cfg, _session_blocks, tree = conftest.build_env(tmp_path)
  node = await conftest.create_task(
      tree, parent=None, request_id="root", profile="manager", task=models.TaskSpec(goal="project"), name="Project")
  dispatch = mock.AsyncMock(return_value={"session_id": node.id, "pending": 0, "launch": False})
  monkeypatch.setattr(tree.dispatch, "dispatch_pending", dispatch)
  trigger = mock.AsyncMock()
  monkeypatch.setattr(_MASTER_TRIGGER_PATCH_TARGET, trigger)

  await tree.dispatch.wake_parent(
      node.id, report=child_report(node.id, outcome="completed", summary="done", event_id="report-node"))

  assert dispatch.await_count == 1
  assert dispatch.await_args.args == (node.id,)
  assert trigger.await_count == 0


@pytest.mark.asyncio
async def test_task_tree_parent_with_the_caller_equal_to_itself_still_dispatches(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The dispatch consults the node's durable inputs, never the caller —
  even when the closer is the node itself."""
  _cfg, _session_blocks, tree = conftest.build_env(tmp_path)
  node = await conftest.create_task(
      tree, parent=None, request_id="root", profile="manager", task=models.TaskSpec(goal="project"), name="Project")
  dispatch = mock.AsyncMock(return_value={"session_id": node.id, "pending": 0, "launch": False})
  monkeypatch.setattr(tree.dispatch, "dispatch_pending", dispatch)
  trigger = mock.AsyncMock()
  monkeypatch.setattr(_MASTER_TRIGGER_PATCH_TARGET, trigger)

  await tree.dispatch.wake_parent(
      node.id,
      report=child_report(node.id, outcome="completed", summary="done", event_id="report-node"),
      caller_session_id=node.id)

  assert dispatch.await_count == 1
  assert dispatch.await_args.args == (node.id,)
  assert trigger.await_count == 0


@pytest.mark.asyncio
async def test_failure_report_dispatches_a_task_tree_parents_pending_inputs(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The first failed/blocked delivery is a task-tree parent's new durable
  input: its next serialized turn dispatches now, and no master wake fires for
  a node parent."""
  cfg, session_blocks, tree = conftest.build_env(tmp_path)
  adapter = conftest.build_execution_adapter(cfg, session_blocks, tree)
  manager = await conftest.create_task(
      tree, parent=None, request_id="root", profile="manager", task=models.TaskSpec(goal="project"), name="Project")
  worker = await conftest.create_task(
      tree, parent=manager.id, request_id="w", profile="worker", task=models.TaskSpec(goal="fix the thing"), name="W")
  await tree.runs.register_run(_failed_work_run(worker.id, "run-t"))
  await tree.runs.record_finish(worker.id, "run-t", "failed")
  run = await tree.runs.get_run(worker.id, "run-t")
  dispatch = mock.AsyncMock(return_value={"session_id": manager.id, "pending": 1, "launch": False})
  monkeypatch.setattr(tree.dispatch, "dispatch_pending", dispatch)
  trigger = mock.AsyncMock()
  monkeypatch.setattr(_MASTER_TRIGGER_PATCH_TARGET, trigger)

  await adapter._report_failure_to_parent(worker.id, run, "failed")

  assert dispatch.await_count == 1
  assert dispatch.await_args.args == (manager.id,)
  assert trigger.await_count == 0
  reports = [e for e in tree.events.load_events(manager.id) if e.get("type") == ET.CHILD_REPORT]
  assert len(reports) == 1 and reports[0]["outcome"] == "failed"
