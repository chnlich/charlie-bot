"""The one parent-wake entry after a report delivery (wake_parent).

A task-tree parent's next serialized turn dispatches from its durable inputs;
a legacy parent (profile None) keeps its own execution path and is woken
through the legacy master wake with the newest child report rendered the way
compose_input_prompt renders a child_report. The report event stays the
durable record either way.
"""

from __future__ import annotations

from datetime import UTC, datetime
from pathlib import Path
from unittest.mock import AsyncMock

import pytest
from conftest import OPUS_BACKEND_ID, make_home_config

from src.core import event_types as ET
from src.core.models import CreateSessionRequest
from src.core.sessions import SessionManager
from src.core.task_sessions import TaskTreeManager


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
async def test_legacy_parent_wakes_through_trigger_master_once(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_mgr, tree = await build_env(tmp_path)
  legacy = await session_mgr.create_session(CreateSessionRequest(name="Legacy"), backend=OPUS_BACKEND_ID)
  child_id = "child-task-1"
  await tree.events.append(
      legacy.id, child_report(child_id, outcome="completed", summary="the work landed",
                              event_id="report-1"))
  trigger = AsyncMock()
  monkeypatch.setattr("src.core.master_trigger.trigger_master", trigger)

  await tree.dispatch.wake_parent(legacy.id)

  assert trigger.await_count == 1
  args = trigger.await_args.args
  assert args[0] == legacy.id
  assert args[1] == f"[Report from task {child_id} | outcome completed] the work landed"
  assert args[2] is cfg
  assert args[3] is session_mgr


@pytest.mark.asyncio
async def test_legacy_parent_wake_uses_the_newest_report(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  _cfg, session_mgr, tree = await build_env(tmp_path)
  legacy = await session_mgr.create_session(CreateSessionRequest(name="Legacy"), backend=OPUS_BACKEND_ID)
  await tree.events.append(
      legacy.id, child_report("child-a", outcome="failed", summary="older attempt",
                              event_id="report-old"))
  await tree.events.append(
      legacy.id, child_report("child-b", outcome="completed", summary="newer attempt",
                              event_id="report-new"))
  trigger = AsyncMock()
  monkeypatch.setattr("src.core.master_trigger.trigger_master", trigger)

  await tree.dispatch.wake_parent(legacy.id)

  assert trigger.await_count == 1
  assert trigger.await_args.args[1] == "[Report from task child-b | outcome completed] newer attempt"


@pytest.mark.asyncio
async def test_legacy_parent_without_a_report_never_wakes(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  _cfg, session_mgr, tree = await build_env(tmp_path)
  legacy = await session_mgr.create_session(CreateSessionRequest(name="Legacy"), backend=OPUS_BACKEND_ID)
  trigger = AsyncMock()
  monkeypatch.setattr("src.core.master_trigger.trigger_master", trigger)

  await tree.dispatch.wake_parent(legacy.id)

  assert trigger.await_count == 0


@pytest.mark.asyncio
async def test_node_parent_dispatches_its_pending_inputs(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  _cfg, _session_mgr, tree = await build_env(tmp_path)
  from src.core.models import TaskSpec
  node = await tree.create_task(
      request_id="root", task_parent_id=None, profile="manager", task=TaskSpec(goal="project"),
      name="Project", backend=None, caller="operator")
  dispatch = AsyncMock(return_value={"session_id": node.id, "pending": 0, "launch": False})
  monkeypatch.setattr(tree.dispatch, "dispatch_pending", dispatch)
  trigger = AsyncMock()
  monkeypatch.setattr("src.core.master_trigger.trigger_master", trigger)

  await tree.dispatch.wake_parent(node.id)

  assert dispatch.await_count == 1
  assert dispatch.await_args.args == (node.id,)
  assert trigger.await_count == 0


@pytest.mark.asyncio
async def test_legacy_chat_view_renders_the_delivered_report(
        tmp_path: Path) -> None:
  """The report event is the durable record the legacy chat view renders: a
  legacy session's aggregated messages include the child report's summary,
  outcome, and node link (message_aggregator's child_report handler)."""
  from src.core.message_aggregator import MessageAggregator

  _cfg, session_mgr, tree = await build_env(tmp_path)
  legacy = await session_mgr.create_session(CreateSessionRequest(name="Legacy"), backend=OPUS_BACKEND_ID)
  await tree.events.append(legacy.id, {
      "id": "u1", "type": ET.USER, "content": "what happened?",
      "timestamp": datetime.now(UTC).isoformat(), "actor": "user",
      "source_session_id": legacy.id,
  })
  report = child_report("child-task-1", outcome="completed", summary="the work landed",
                        event_id="report-1")
  await tree.events.append(legacy.id, report)

  events = session_mgr.load_chat_events_sync(legacy.id)
  agg = MessageAggregator()
  messages = [d["message"] for event in events for d in agg.feed(event)
              if d.get("type") == "message"]

  reports = [m for m in messages if m.get("role") == ET.CHILD_REPORT]
  assert len(reports) == 1
  assert reports[0]["content"] == "the work landed"
  assert reports[0]["outcome"] == "completed"
  assert reports[0]["child_session_id"] == "child-task-1"


@pytest.mark.asyncio
async def test_missing_parent_logs_and_returns(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  _cfg, _session_mgr, tree = await build_env(tmp_path)
  dispatch = AsyncMock()
  monkeypatch.setattr(tree.dispatch, "dispatch_pending", dispatch)
  trigger = AsyncMock()
  monkeypatch.setattr("src.core.master_trigger.trigger_master", trigger)

  await tree.dispatch.wake_parent("no-such-parent")

  assert dispatch.await_count == 0 and trigger.await_count == 0
