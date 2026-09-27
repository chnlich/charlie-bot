"""The one parent-wake entry after a report delivery (wake_parent).

A task-tree parent's next serialized turn dispatches from its durable inputs;
a legacy parent (profile None) is woken through the legacy master wake with
the report the caller passed in, rendered the way compose_input_prompt renders
a child_report — unless the caller session is the parent itself, whose own
turn already holds the outcome in its HTTP response. The report event stays
the durable record either way.
"""

from __future__ import annotations

import asyncio
from datetime import UTC, datetime
from pathlib import Path
from unittest.mock import AsyncMock

import pytest
from conftest import MASTER_TRIGGER_TRIGGER_MASTER_PATCH_TARGET, OPUS_BACKEND_ID, make_home_config

from src.core import event_types as ET
from src.core.models import CreateSessionRequest
from src.core.sessions import SessionManager
from src.core.task_sessions import TaskTreeManager


async def await_wake(task: asyncio.Task | None) -> None:
  """The legacy wake is scheduled, not awaited inline; the test waits it out."""
  assert task is not None
  await task


async def build_env(tmp_path: Path):
  cfg = make_home_config(tmp_path)
  session_mgr = SessionManager(cfg)
  tree = TaskTreeManager(cfg, session_mgr)
  return cfg, session_mgr, tree


def child_report(child_session_id: str, *, outcome: str, summary: str, event_id: str) -> dict:
  return {
      "id": event_id,
      "type": ET.CHILD_REPORT,
      "timestamp": datetime.now(UTC).isoformat(),
      "actor": "system",
      "source_session_id": child_session_id,
      "child_session_id": child_session_id,
      "child_event_id": "close-1",
      "outcome": outcome,
      "summary": summary,
      "result_refs": [],
  }


@pytest.mark.asyncio
async def test_legacy_parent_wakes_through_trigger_master_once(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_mgr, tree = await build_env(tmp_path)
  legacy = await session_mgr.create_session(CreateSessionRequest(name="Legacy"), backend=OPUS_BACKEND_ID)
  child_id = "child-task-1"
  report = child_report(child_id, outcome="completed", summary="the work landed", event_id="report-1")
  await tree.events.append(legacy.id, report)
  trigger = AsyncMock()
  monkeypatch.setattr(MASTER_TRIGGER_TRIGGER_MASTER_PATCH_TARGET, trigger)

  await await_wake(await tree.dispatch.wake_parent(legacy.id, report=report))

  assert trigger.await_count == 1
  args = trigger.await_args.args
  assert args[0] == legacy.id
  assert args[1] == f"[Report from task {child_id} | outcome completed] the work landed"
  assert args[2] is cfg
  assert args[3] is session_mgr


@pytest.mark.asyncio
async def test_legacy_parent_closed_by_its_own_turn_skips_the_wake(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The parent's own turn already holds the outcome in its HTTP response; an
  echo wake would only replay the parent's own words as a new queued turn."""
  _cfg, session_mgr, tree = await build_env(tmp_path)
  legacy = await session_mgr.create_session(CreateSessionRequest(name="Legacy"), backend=OPUS_BACKEND_ID)
  report = child_report("child-task-1", outcome="cancelled", summary="no longer needed", event_id="report-1")
  await tree.events.append(legacy.id, report)
  trigger = AsyncMock()
  monkeypatch.setattr(MASTER_TRIGGER_TRIGGER_MASTER_PATCH_TARGET, trigger)

  task = await tree.dispatch.wake_parent(legacy.id, report=report, caller_session_id=legacy.id)

  assert task is None
  assert trigger.await_count == 0
  # The report stays the durable record in the parent's fact history.
  assert [e["id"] for e in tree.fact_history(legacy.id) if e.get("type") == ET.CHILD_REPORT] == ["report-1"]


@pytest.mark.asyncio
async def test_node_parent_dispatches_its_pending_inputs(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  _cfg, _session_mgr, tree = await build_env(tmp_path)
  from src.core.models import TaskSpec
  node = await tree.create_task(
      request_id="root",
      task_parent_id=None,
      profile="manager",
      task=TaskSpec(goal="project"),
      name="Project",
      backend=None,
      caller="operator")
  dispatch = AsyncMock(return_value={"session_id": node.id, "pending": 0, "launch": False})
  monkeypatch.setattr(tree.dispatch, "dispatch_pending", dispatch)
  trigger = AsyncMock()
  monkeypatch.setattr(MASTER_TRIGGER_TRIGGER_MASTER_PATCH_TARGET, trigger)

  await tree.dispatch.wake_parent(
      node.id, report=child_report(node.id, outcome="completed", summary="done", event_id="report-node"))

  assert dispatch.await_count == 1
  assert dispatch.await_args.args == (node.id,)
  assert trigger.await_count == 0


@pytest.mark.asyncio
async def test_task_tree_parent_with_the_caller_equal_to_itself_still_dispatches(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The skip rule is legacy-only: a manager node's dispatch consults its
  durable inputs, never the caller — even when the closer is the node itself."""
  from src.core.models import TaskSpec
  _cfg, _session_mgr, tree = await build_env(tmp_path)
  node = await tree.create_task(
      request_id="root",
      task_parent_id=None,
      profile="manager",
      task=TaskSpec(goal="project"),
      name="Project",
      backend=None,
      caller="operator")
  dispatch = AsyncMock(return_value={"session_id": node.id, "pending": 0, "launch": False})
  monkeypatch.setattr(tree.dispatch, "dispatch_pending", dispatch)
  trigger = AsyncMock()
  monkeypatch.setattr(MASTER_TRIGGER_TRIGGER_MASTER_PATCH_TARGET, trigger)

  await tree.dispatch.wake_parent(
      node.id,
      report=child_report(node.id, outcome="completed", summary="done", event_id="report-node"),
      caller_session_id=node.id)

  assert dispatch.await_count == 1
  assert dispatch.await_args.args == (node.id,)
  assert trigger.await_count == 0
