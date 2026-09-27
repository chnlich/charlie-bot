"""Dispatch-stage tests: durable input, claims, parent reports, recovery over real on-disk state."""

from __future__ import annotations

from datetime import UTC, datetime
from pathlib import Path

import pytest
from conftest import build_env, create_task

from src.api.message_utils import events_to_view
from src.core import event_types as ET
from src.core.models import RunRecord
from src.core.run_token import CallerIdentity, RunTokenClaims
from src.core.sessions import SessionManager
from src.core.task_sessions import (
    TaskConflictError,
    TaskForbiddenError,
    TaskTreeManager,
)

OPERATOR = CallerIdentity(kind="operator")


async def admit(
    tree: TaskTreeManager,
    session_id: str,
    content: str,
    *,
    event_type: str = ET.USER,
    actor: str = "user",
    input_id: str | None = None,
    **payload: object) -> dict:
  return await tree.dispatch.admit_input(
      session_id, event_type=event_type, content=content, actor=actor, input_id=input_id, **payload)


class ScriptedExecutor:
  """The deterministic executor seam: binds one queued Run per dispatch and
  finishes it with the scripted outcome (or leaves it queued when finish=False)."""

  def __init__(self, tree: TaskTreeManager, *, outcome: str = "success", finish: bool = True) -> None:
    self.tree = tree
    self.outcome = outcome
    self.finish = finish
    self.batches: list[tuple[str, list[str]]] = []
    self.counter = 0

  async def __call__(self, session_id: str, pending: list[dict]) -> str:
    self.counter += 1
    batch_ids = [str(e["id"]) for e in pending]
    self.batches.append((session_id, batch_ids))
    run = await self.tree.runs.register_run(
        RunRecord(id=f"consumer-{self.counter}", session_id=session_id, kind="work"))
    bound = await self.tree.dispatch.claim_input_batch(session_id, run.id)
    assert bound == batch_ids
    if self.finish:
      await self.tree.dispatch.finish_run(session_id, run.id, outcome=self.outcome)
    return run.id


def child_report_messages(session_mgr: SessionManager, session_id: str) -> list[dict]:
  events = session_mgr.load_chat_events_sync(session_id)
  view, _draft = events_to_view(events)
  return [m for m in view if m.get("role") == ET.CHILD_REPORT]


def input_events(tree: TaskTreeManager, session_id: str) -> list[dict]:
  return tree.dispatch.pending_inputs(session_id)


# ---------------------------------------------------------------------------
# Parent reports: eligibility, dedup, crash windows
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_two_reports_one_acknowledged_reload_leaves_only_the_other(tmp_path: Path) -> None:
  cfg, session_mgr, tree = build_env(tmp_path)
  parent = await create_task(tree, parent=None, request_id="parent")
  child_a = await create_task(tree, parent=parent.id, request_id="a", profile="worker")
  child_b = await create_task(tree, parent=parent.id, request_id="b", profile="worker")

  # Child A closes and reports; the parent's first consumer claims exactly that
  # report and finishes successfully, acknowledging only it.
  run_a = await tree.runs.register_run(RunRecord(id="run-a", session_id=child_a.id, kind="work"))
  await tree.dispatch.finish_run(child_a.id, run_a.id, outcome="success")  # auto-close + report
  assert input_events(tree, parent.id) and {e["child_session_id"] for e in input_events(tree, parent.id)
                                           } == {child_a.id}

  executor = ScriptedExecutor(tree)
  tree.dispatch.executor = executor
  await tree.dispatch.dispatch_pending(parent.id)
  assert len(executor.batches) == 1 and executor.batches[0][0] == parent.id
  acknowledged = executor.batches[0][1]
  assert len(acknowledged) == 1
  events = tree.events.load_events(parent.id)
  finished = [e for e in events if e["type"] == ET.RUN_FINISHED and e.get("outcome") == "success"]
  assert finished and set(finished[-1]["input_event_ids"]) == set(acknowledged)

  # Child B closes after the parent's run: a later arrival kept for the next run.
  # With no executor installed the close-time report dispatch records the
  # report only (the execution adapter's own tests cover the live dispatch).
  tree.dispatch.executor = None
  run_b = await tree.runs.register_run(RunRecord(id="run-b", session_id=child_b.id, kind="work"))
  await tree.dispatch.finish_run(child_b.id, run_b.id, outcome="success")
  pending_now = {e["child_session_id"] for e in input_events(tree, parent.id)}
  assert pending_now == {child_b.id}

  # A fresh instance derives the same eligibility from disk facts alone.
  fresh = TaskTreeManager(cfg, session_mgr)
  pending_fresh = {e["child_session_id"] for e in input_events(fresh, parent.id)}
  assert pending_fresh == {child_b.id}

  # Duplicate scans and delivery retries produce one parent event and one
  # message per child: the stable report id dedups persistence and display.
  await fresh.dispatch.recover_pending_reports(child_a.id)
  await fresh.dispatch.recover_pending_reports(child_b.id)
  await fresh.dispatch.recover_pending_reports(child_b.id)
  events = fresh.events.load_events(parent.id)
  reports = [e for e in events if e["type"] == ET.CHILD_REPORT]
  assert sorted(str(e["child_session_id"]) for e in reports) == sorted([child_a.id, child_b.id])
  assert len(child_report_messages(session_mgr, parent.id)) == 2


@pytest.mark.asyncio
async def test_delivery_crash_windows_repair_after_a_fresh_instance(tmp_path: Path) -> None:
  cfg, session_mgr, tree = build_env(tmp_path)
  parent = await create_task(tree, parent=None, request_id="parent")
  child = await create_task(tree, parent=parent.id, request_id="child", profile="worker")

  # Window (a): the child's close fact landed but the parent append was lost.
  close_event = {
      "id": "close-child",
      "type": ET.TASK_CLOSED,
      "timestamp": datetime.now(UTC).isoformat(),
      "actor": "system",
      "source_session_id": child.id,
      "request_id": "auto:run-1",
      "outcome": "completed",
      "summary": "delivered",
      "result_refs": ["run:run-1"],
      "run_ids": ["run-1"],
      "report_to": parent.id,
  }
  await tree.events.append(child.id, close_event)
  fresh = TaskTreeManager(cfg, session_mgr)

  # While the report is undelivered, the moving-subtree reparent guard blocks.
  from src.core.models import PatchSessionTaskRequest
  other_root = await fresh.create_task(
      request_id="other-root",
      task_parent_id=None,
      profile="manager",
      task=None,
      name=None,
      backend=None,
      caller=OPERATOR)
  with pytest.raises(TaskConflictError, match="undelivered parent report"):
    await fresh.patch_task(child.id, PatchSessionTaskRequest(task_parent_id=other_root.id), caller=OPERATOR)

  delivered = await fresh.dispatch.recover_pending_reports(child.id)
  assert len(delivered) == 1 and delivered[0]["child_session_id"] == child.id
  assert len(child_report_messages(session_mgr, parent.id)) == 1

  # A repeat recovery pass and a direct re-delivery both dedup to the same event.
  again = await fresh.dispatch.recover_pending_reports(child.id)
  assert again == []
  redelivered = await fresh.dispatch.deliver_child_report(
      child.id, source_event=close_event, outcome="completed", summary="delivered", result_refs=[], recipient=parent.id)
  assert redelivered is not None and redelivered["id"] == delivered[0]["id"]
  assert len(child_report_messages(session_mgr, parent.id)) == 1

  # Window (b): the parent append landed but the enqueue was lost — the report
  # is a pending input the fresh instance's recovery calculation rediscovers.
  pending = input_events(fresh, parent.id)
  assert [str(e["id"]) for e in pending] == [delivered[0]["id"]]

  # With the report delivered, the same move passes its report guards.
  await fresh.patch_task(child.id, PatchSessionTaskRequest(task_parent_id=other_root.id), caller=OPERATOR)
  moved = await fresh.load_meta(child.id)
  assert moved is not None and moved.task_parent_id == other_root.id


# ---------------------------------------------------------------------------
# Later input, acknowledgement exactness
# ---------------------------------------------------------------------------

# ---------------------------------------------------------------------------
# Rotation, imported boundary, close/reopen across segments
# ---------------------------------------------------------------------------

# ---------------------------------------------------------------------------
# Closed nodes, authorization identities, stopped queued runs
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_stopped_queued_run_is_never_launched_and_releases_its_batch(tmp_path: Path) -> None:
  _, _, tree = build_env(tmp_path)
  node = await create_task(tree, parent=None, request_id="node")
  await admit(tree, node.id, "work to do", input_id="job-1")
  queued = await tree.runs.register_run(RunRecord(id="run-queued", session_id=node.id))
  await tree.dispatch.claim_input_batch(node.id, queued.id)
  stop = await tree.runs.request_stop(node.id, "run-queued", "stop-1")
  assert stop.stop_requested is True and stop.outcome is None  # no exit was observed

  executor = ScriptedExecutor(tree)
  tree.dispatch.executor = executor
  decision = await tree.dispatch.dispatch_pending(node.id)
  assert decision["launch"] is True  # the stopped queued run never consumes
  assert executor.batches and executor.batches[0][1] == ["job-1"]

  with pytest.raises(TaskConflictError, match="stop request"):
    await tree.dispatch.claim_input_batch(node.id, "run-queued")


# ---------------------------------------------------------------------------
# Input candidacy and handling: tool echoes, stopped/failed/interrupted rounds
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_legacy_trigger_wake_is_refused_at_v2_nodes(tmp_path: Path) -> None:
  """A scheduled/iteration wake aimed at a v2 node is explicitly unavailable (next stage):
  nothing is written through the legacy user-event path and nothing dispatches."""
  cfg, session_mgr, tree = build_env(tmp_path)
  manager = await create_task(tree, parent=None, request_id="root", name="Root")

  executor = ScriptedExecutor(tree)
  tree.dispatch.executor = executor
  from src.core.master_trigger import trigger_master
  await trigger_master(manager.id, "a worker result landed", cfg, session_mgr)

  events = tree.events.load_events(manager.id)
  assert [e for e in events if e["type"] == ET.USER] == []
  assert executor.batches == []
  assert tree.runs.list_run_records_sync(manager.id) == []

  # A v1 session keeps the legacy wake path intact during the staged conversion.
  from src.core.models import CreateSessionRequest
  v1 = await session_mgr.create_session(CreateSessionRequest(name="legacy"))
  called: list[bool] = []

  async def _fake_run_message(*args, **kwargs):
    called.append(True)
    return "cc-1"

  import src.core.master_trigger as master_trigger_module
  original = master_trigger_module.run_message
  master_trigger_module.run_message = _fake_run_message
  try:
    await trigger_master(v1.id, "a worker result landed", cfg, session_mgr)
  finally:
    master_trigger_module.run_message = original
  assert called == [True]


# ---------------------------------------------------------------------------
# Input identity, dedup, and attachment preservation
# ---------------------------------------------------------------------------

# ---------------------------------------------------------------------------
# Index races: phantom nodes and stale installs
# ---------------------------------------------------------------------------

# ---------------------------------------------------------------------------
# Permanent delete: every reference category, one locked operation
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_deletion_rejects_each_reference_category_and_deletes_the_empty(tmp_path: Path) -> None:
  _cfg, session_mgr, tree = build_env(tmp_path)
  root = await create_task(tree, parent=None, request_id="root")
  child = await create_task(tree, parent=root.id, request_id="child", profile="worker")

  # Child reference.
  blockers = await tree.deletion_blockers(root.id)
  assert any("child task" in b for b in blockers)
  # Runs.
  await tree.runs.register_run(RunRecord(id="run-blk", session_id=child.id))
  blockers = await tree.deletion_blockers(child.id)
  assert any("run record" in b for b in blockers)
  # Preserved conversation beyond the creation fact.
  await admit(tree, child.id, "history", input_id="hist-1")
  blockers = await tree.deletion_blockers(child.id)
  assert any("preserved conversation" in b for b in blockers)
  # A child report held in another task's log.
  other_root = await create_task(tree, parent=None, request_id="other-root")
  await tree.dispatch.deliver_child_report(
      child.id, source_event={"id": "src-1"}, outcome="failed", summary="s", result_refs=[], recipient=other_root.id)
  blockers = await tree.deletion_blockers(child.id)
  assert any("child report" in b for b in blockers)
  # An origin reference from another task's saved metadata.
  from src.core.models import EventRef
  fork_meta = await session_mgr.get_session(other_root.id)
  assert fork_meta is not None
  fork_meta.origin_ref = EventRef(session_id=child.id, event_id=None)
  await session_mgr.save_metadata(fork_meta)
  blockers = await tree.deletion_blockers(child.id)
  assert any("origin_ref" in b for b in blockers)

  # A truly empty, unreferenced task is deletable under the same lock.
  empty = await create_task(tree, parent=None, request_id="empty")
  assert await tree.delete_permanently(empty.id, caller=OPERATOR) is True
  assert await session_mgr.get_session(empty.id) is None
  # Agents cannot delete.
  with pytest.raises(TaskForbiddenError):
    await tree.delete_permanently(
        other_root.id,
        caller=CallerIdentity(kind="run", claims=RunTokenClaims(run_id="r", session_id=other_root.id, agent="a")))


# ---------------------------------------------------------------------------
# Real routes: browser and agent-relay input through the dispatcher
# ---------------------------------------------------------------------------
