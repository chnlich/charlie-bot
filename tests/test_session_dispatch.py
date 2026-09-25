"""Dispatch-stage tests: durable input, claims, parent reports, recovery over real on-disk state."""

from __future__ import annotations

import asyncio
import os
import subprocess
import threading
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest
from conftest import make_home_config, stub_credentials

from src.api.message_utils import events_to_view
from src.core import event_types as ET
from src.core.models import RunRecord, TaskSpec
from src.core.run_token import CallerIdentity, RunTokenClaims, sign_run_token
from src.core.runs import read_pid_stat
from src.core.sessions import SessionManager
from src.core.task_completion import CompletionEvidence
from src.core.task_sessions import (
  TaskConflictError,
  TaskForbiddenError,
  TaskTreeManager,
)

OPERATOR = CallerIdentity(kind="operator")

# The Claude CLI's persisted tool-result echo: a user-type event whose content
# is a list of tool_result blocks (src/cli/claude_sub_bridge.py), the shape
# production logs carry for every tool call.
TOOL_RESULT_ECHO = {
    "type": ET.USER,
    "message": {
        "role": "user",
        "content": [{
            "type": ET.TOOL_RESULT,
            "tool_use_id": "toolu_1",
            "content": "ok",
        }],
    },
    "parent_tool_use_id": None,
    "session_id": "cc-session-1",
    "uuid": "0b6c7f1e-0000-4000-8000-000000000001",
    "tool_use_result": {"stdout": "ok", "stderr": ""},
}


def live_subprocess() -> subprocess.Popen:
  """An owned, isolated sleeper: the only process identity any test here signals."""
  return subprocess.Popen(["/bin/sleep", "30"])


def identity_of(pid: int) -> tuple[int, str]:
  pair = read_pid_stat(pid)
  assert pair is not None
  return pid, pair[0]


def build_env(tmp_path: Path) -> tuple[object, SessionManager, TaskTreeManager]:
  cfg = make_home_config(tmp_path)
  session_mgr = SessionManager(cfg)
  return cfg, session_mgr, TaskTreeManager(cfg, session_mgr)


async def create_task(tree: TaskTreeManager, *, parent: str | None, request_id: str,
                      profile: str = "manager", task: TaskSpec | None = None,
                      name: str | None = None):
  return await tree.create_task(
      request_id=request_id, task_parent_id=parent, profile=profile, task=task,
      name=name, backend=None, caller=OPERATOR)


async def admit(tree: TaskTreeManager, session_id: str, content: str, *, event_type: str = ET.USER,
                actor: str = "user", input_id: str | None = None, **payload: object) -> dict:
  return await tree.dispatch.admit_input(
      session_id, event_type=event_type, content=content, actor=actor,
      input_id=input_id, **payload)


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
  assert input_events(tree, parent.id) and {e["child_session_id"] for e in input_events(tree, parent.id)} == {child_a.id}

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
      "id": "close-child", "type": ET.TASK_CLOSED,
      "timestamp": datetime.now(UTC).isoformat(), "actor": "system",
      "source_session_id": child.id, "request_id": "auto:run-1",
      "outcome": "completed", "summary": "delivered", "result_refs": ["run:run-1"],
      "run_ids": ["run-1"], "report_to": parent.id,
  }
  await tree.events.append(child.id, close_event)
  fresh = TaskTreeManager(cfg, session_mgr)

  # While the report is undelivered, the moving-subtree reparent guard blocks.
  from src.core.models import PatchSessionTaskRequest
  other_root = await fresh.create_task(
      request_id="other-root", task_parent_id=None, profile="manager", task=None,
      name=None, backend=None, caller=OPERATOR)
  with pytest.raises(TaskConflictError, match="undelivered parent report"):
    await fresh.patch_task(
        child.id, PatchSessionTaskRequest(task_parent_id=other_root.id), caller=OPERATOR)

  delivered = await fresh.dispatch.recover_pending_reports(child.id)
  assert len(delivered) == 1 and delivered[0]["child_session_id"] == child.id
  assert len(child_report_messages(session_mgr, parent.id)) == 1

  # A repeat recovery pass and a direct re-delivery both dedup to the same event.
  again = await fresh.dispatch.recover_pending_reports(child.id)
  assert again == []
  redelivered = await fresh.dispatch.deliver_child_report(
      child.id, source_event=close_event, outcome="completed", summary="delivered",
      result_refs=[], recipient=parent.id)
  assert redelivered is not None and redelivered["id"] == delivered[0]["id"]
  assert len(child_report_messages(session_mgr, parent.id)) == 1

  # Window (b): the parent append landed but the enqueue was lost — the report
  # is a pending input the fresh instance's recovery calculation rediscovers.
  pending = input_events(fresh, parent.id)
  assert [str(e["id"]) for e in pending] == [delivered[0]["id"]]

  # With the report delivered, the same move passes its report guards.
  await fresh.patch_task(
      child.id, PatchSessionTaskRequest(task_parent_id=other_root.id), caller=OPERATOR)
  moved = await fresh.load_meta(child.id)
  assert moved is not None and moved.task_parent_id == other_root.id


# ---------------------------------------------------------------------------
# Later input, acknowledgement exactness
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_later_input_during_execution_keeps_id_and_blocks_close(tmp_path: Path) -> None:
  _, _session_mgr, tree = build_env(tmp_path)
  parent_task = await create_task(tree, parent=None, request_id="parent")
  node = await create_task(tree, parent=parent_task.id, request_id="node", profile="worker")
  first = await admit(tree, node.id, "first input", input_id="input-1")
  executor = ScriptedExecutor(tree, finish=False)
  tree.dispatch.executor = executor
  await tree.dispatch.dispatch_pending(node.id)
  run_id = executor.tree.runs.list_run_records_sync(node.id)[0].id

  later = await admit(tree, node.id, "later input", input_id="input-2")
  # The exact batch stays bound to its Run; the later arrival keeps its own id.
  events = tree.events.load_events(node.id)
  finished_or_running = [e for e in events if e["type"] == ET.RUN_FINISHED]
  assert not finished_or_running
  pending_ids = [str(e["id"]) for e in input_events(tree, node.id)]
  assert pending_ids == ["input-2"]
  assert first["id"] == "input-1" and later["id"] == "input-2"

  # Both automatic and manual closure stay open on the later arrival: the
  # successful run acknowledges only its batch, the automatic close is blocked,
  # and the later input keeps its id.
  await tree.dispatch.finish_run(node.id, run_id, outcome="success")
  assert tree.task_state(node.id) == "open"
  assert {str(e["id"]) for e in input_events(tree, node.id)} == {"input-2"}
  with pytest.raises(TaskConflictError, match="unprocessed input"):
    await tree.completion.complete_task(
        node.id, request_id="manual-1",
        evidence=CompletionEvidence(summary="s", result_refs=[], run_ids=[]),
        caller=OPERATOR)

  # The success acknowledged exactly its claimed batch; the later arrival is
  # all that remains pending.
  assert {str(e["id"]) for e in input_events(tree, node.id)} == {"input-2"}

  # An unrelated MASTER_DONE output cannot consume an input.
  await tree.events.append(node.id, {"type": ET.MASTER_DONE, "thinking_seconds": 3})
  assert {str(e["id"]) for e in input_events(tree, node.id)} == {"input-2"}

  # A failed round counts as handled whatever its outcome: its claimed batch
  # never reappears as pending, and no retry is needed before the next input
  # dispatches (the launched round's outcome is the handling).
  failed_run = await tree.runs.register_run(RunRecord(id="run-failed", session_id=node.id))
  await tree.dispatch.claim_input_batch(node.id, failed_run.id, input_ids=["input-2"])
  await tree.dispatch.finish_run(node.id, failed_run.id, outcome="failed")
  assert input_events(tree, node.id) == []
  await admit(tree, node.id, "next instruction", input_id="input-3")
  decision = await tree.dispatch.dispatch_pending(node.id)
  assert decision["launch"] is True
  assert executor.batches[-1] == (node.id, ["input-3"])


@pytest.mark.asyncio
async def test_exact_per_run_batch_validation_rejects_foreign_ids(tmp_path: Path) -> None:
  from src.core.runs import RunInputMismatchError

  _, _, tree = build_env(tmp_path)
  node = await create_task(tree, parent=None, request_id="node")
  await admit(tree, node.id, "claimed input", input_id="e-claimed")
  await admit(tree, node.id, "later input", input_id="e-later")
  run = await tree.runs.register_run(RunRecord(id="run-1", session_id=node.id, kind="work"))
  bound = await tree.dispatch.claim_input_batch(node.id, run.id, input_ids=["e-claimed"])
  assert bound == ["e-claimed"]

  # Finishing with the registered batch is the acknowledged payload.
  await tree.dispatch.finish_run(node.id, run.id, outcome="success", input_event_ids=["e-claimed"])
  finished = [e for e in tree.events.load_events(node.id) if e["type"] == ET.RUN_FINISHED]
  assert finished[0]["input_event_ids"] == ["e-claimed"]

  # A second run cannot acknowledge the first run's input: finish_run checks
  # the pending claim set, so a foreign id never lands as a terminal fact.
  other = await tree.runs.register_run(RunRecord(id="run-2", session_id=node.id, kind="work"))
  with pytest.raises(TaskConflictError, match="not pending"):
    await tree.dispatch.finish_run(node.id, other.id, outcome="success", input_event_ids=["e-claimed"])
  # ...and the primitive writer rejects ids outside a registered batch outright.
  with pytest.raises(RunInputMismatchError):
    await tree.runs.record_finish(node.id, other.id, "success", input_event_ids=["e-claimed"])
  assert not [e for e in tree.events.load_events(node.id)
              if e["type"] == ET.RUN_FINISHED and e.get("run_id") == "run-2"]


@pytest.mark.asyncio
async def test_finish_payload_fills_from_the_registered_batch(tmp_path: Path) -> None:
  _, _, tree = build_env(tmp_path)
  node = await create_task(tree, parent=None, request_id="node")
  await admit(tree, node.id, "batched input", input_id="e-1")
  run = await tree.runs.register_run(RunRecord(id="run-batched", session_id=node.id))
  await tree.dispatch.claim_input_batch(node.id, run.id)
  # A permissive caller default of [] must not shrink the acknowledgement.
  await tree.dispatch.finish_run(node.id, run.id, outcome="success", input_event_ids=[])
  finished = [e for e in tree.events.load_events(node.id) if e["type"] == ET.RUN_FINISHED]
  assert finished[0]["input_event_ids"] == ["e-1"]
  record = await tree.runs.get_run(node.id, "run-batched")
  assert record is not None and record.input_event_ids == ["e-1"]
  assert {str(e["id"]) for e in input_events(tree, node.id)} == set()


# ---------------------------------------------------------------------------
# Rotation, imported boundary, close/reopen across segments
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_rotation_preserves_pending_import_close_and_acknowledgement_facts(tmp_path: Path) -> None:
  cfg, session_mgr, tree = build_env(tmp_path)
  node = await create_task(tree, parent=None, request_id="node")
  original_time = datetime.now(UTC) - timedelta(hours=2)

  await admit(tree, node.id, "take off and build it", input_id="e-answered",
              timestamp=original_time.isoformat())
  run = await tree.runs.register_run(RunRecord(id="run-old", session_id=node.id))
  await tree.dispatch.claim_input_batch(node.id, run.id)
  await tree.dispatch.finish_run(node.id, run.id, outcome="success")

  pending_input = await admit(tree, node.id, "still unanswered", input_id="e-pending",
                              timestamp=(original_time + timedelta(minutes=1)).isoformat())
  # An old input that migration judged handled-but-unprovable is NOT declared
  # pending; the import boundary must exclude it from candidacy.
  await admit(tree, node.id, "unlisted old input", input_id="e-unlisted",
              timestamp=(original_time + timedelta(minutes=2)).isoformat())
  # A historical close and reopen straddle the facts, then the import boundary
  # declares exactly the still-pending old input.
  await tree.events.append(node.id, {
      "id": "close-old", "type": ET.TASK_CLOSED, "timestamp": datetime.now(UTC).isoformat(),
      "actor": "user", "source_session_id": node.id, "request_id": "close-1",
      "outcome": "completed", "summary": "s", "result_refs": [], "run_ids": [], "report_to": None,
  })
  await tree.events.append(node.id, {
      "id": "import-1", "type": ET.TASK_IMPORTED, "timestamp": datetime.now(UTC).isoformat(),
      "actor": "system", "source_session_id": node.id,
      "source_refs": ["old-session"], "pending_inputs": [
          {"source_ref": f"old-session#{pending_input['id']}", "input_id": pending_input["id"]}],
  })
  await tree.events.append(node.id, {
      "id": "reopen-1", "type": ET.TASK_REOPENED, "timestamp": datetime.now(UTC).isoformat(),
      "actor": "user", "source_session_id": node.id, "request_id": "reopen-1",
      "closed_event_id": "close-old", "reason": "more work arrived",
  })

  before_pending = [(str(e["id"]), e["type"], e["timestamp"]) for e in input_events(tree, node.id)]
  before_state = tree.task_state(node.id)
  assert before_pending == [("e-pending", ET.USER, pending_input["timestamp"])]
  assert "e-unlisted" not in {str(e["id"]) for e in input_events(tree, node.id)}  # only declared old pending
  assert before_state == "open"

  # Rotate: every event so far moves into the weekly archive segment.
  await session_mgr.recycle_scheduled_session(node.id, datetime.now(UTC) + timedelta(hours=1))
  rotated = await session_mgr.get_session(node.id)
  assert rotated is not None and rotated.archive_offset > 0  # the live log is now empty

  fresh = TaskTreeManager(cfg, session_mgr)
  after_pending = [(str(e["id"]), e["type"], e["timestamp"]) for e in input_events(fresh, node.id)]
  assert after_pending == before_pending  # same ids, original user times/types
  assert fresh.task_state(node.id) == "open"
  # The answered historical input stays handled across rotation.
  assert "e-answered" not in {str(e["id"]) for e in input_events(fresh, node.id)}
  detail = await fresh.session_detail(node.id)
  assert detail["task_state"] == "open" and detail["archived"] is False


# ---------------------------------------------------------------------------
# Pause, closed nodes, authorization identities, stopped queued runs
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_pause_is_local_and_holds_input_without_launch(tmp_path: Path) -> None:
  _, _, tree = build_env(tmp_path)
  paused = await create_task(tree, parent=None, request_id="paused")
  sibling = await create_task(tree, parent=None, request_id="sibling")
  from src.core.models import PatchSessionTaskRequest
  await tree.patch_task(paused.id, PatchSessionTaskRequest(automation_paused=True), caller=OPERATOR)

  executor = ScriptedExecutor(tree)
  tree.dispatch.executor = executor
  await admit(tree, paused.id, "held input", input_id="held-1")
  decision = await tree.dispatch.dispatch_pending(paused.id)
  assert decision["launch"] is False and "automation_paused" in decision["reason"]
  assert executor.batches == []  # nothing launched on the paused node
  assert [str(e["id"]) for e in input_events(tree, paused.id)] == ["held-1"]

  # The sibling continues independently.
  await admit(tree, sibling.id, "go", input_id="sib-1")
  await tree.dispatch.dispatch_pending(sibling.id)
  assert [b[0] for b in executor.batches] == [sibling.id]

  # Resume releases the preserved inputs to the executor.
  await tree.patch_task(paused.id, PatchSessionTaskRequest(automation_paused=False), caller=OPERATOR)
  await tree.dispatch.dispatch_pending(paused.id)
  assert [b[0] for b in executor.batches] == [sibling.id, paused.id]
  assert executor.batches[-1][1] == ["held-1"]


@pytest.mark.asyncio
async def test_closed_node_keeps_input_and_agent_content_never_mints_authorization(tmp_path: Path) -> None:
  _, _, tree = build_env(tmp_path)
  root = await create_task(tree, parent=None, request_id="root")
  worker = await create_task(tree, parent=root.id, request_id="worker", profile="worker")

  # Agent and cron input carry their own types even with takeoff in the text;
  # they never mint a user authorization window.
  await admit(tree, worker.id, "take off now", event_type=ET.AGENT_MESSAGE, actor="agent",
              from_session=root.id)
  await admit(tree, worker.id, "take off (cron)", event_type=ET.SCHEDULED_TRIGGER, actor="system")
  from src.core.takeoff_gate import DelegationBlockedError
  with pytest.raises(DelegationBlockedError):
    await tree.check_task_authorization(worker.id)

  # A consumer takes the agent/cron inputs; with the node's inputs handled the
  # successful run auto-closes the worker.
  executor = ScriptedExecutor(tree)
  tree.dispatch.executor = executor
  await tree.dispatch.dispatch_pending(worker.id)
  assert len(executor.batches) == 2 and len(executor.batches[0][1]) == 2
  # The delivered close report is the parent's durable input: the close itself
  # dispatched the parent's next serialized turn through the same executor.
  assert executor.batches[1][0] == root.id and len(executor.batches[1][1]) == 1
  index = await tree._get_index()
  assert tree.task_state_of(index, worker.id) == "completed"  # auto-close landed

  # The closed node keeps late input as history and attention-to-view.
  await admit(tree, worker.id, "late arrival", input_id="late-1")
  decision = await tree.dispatch.dispatch_pending(worker.id)
  assert decision["launch"] is False and "closed" in decision["reason"]
  assert [str(e["id"]) for e in input_events(tree, worker.id)] == ["late-1"]
  meta = await tree.load_meta(worker.id)
  assert meta is not None and meta.automation_paused is False  # close never flips pause

  # A later authorized user retry uses the preserved inputs.
  await admit(tree, worker.id, "take off — redo it", input_id="user-retry-1")
  with pytest.raises(DelegationBlockedError):
    # the node is still closed: the gate requires open tasks before any launch
    await tree.check_task_authorization(worker.id)


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
async def test_cli_tool_result_echo_is_not_pending_input_and_never_blocks_close(
    tmp_path: Path) -> None:
  """The Claude CLI persists each tool result as a user-type event whose
  content is a list of tool_result blocks (src/cli/claude_sub_bridge.py).
  That echo is tool output, not a message: it enters no round's batch, stays
  out of the pending set after the round that produced it, and never blocks
  the task from closing (the probe_tool_echo.py scenario)."""
  _, session_mgr, tree = build_env(tmp_path)
  node = await create_task(tree, parent=None, request_id="node")
  await admit(tree, node.id, "please run ls")
  run = await tree.runs.register_run(
      RunRecord(id="probe-run", session_id=node.id, kind="manager_turn"))
  bound = await tree.dispatch.claim_input_batch(node.id, run.id)
  assert len(bound) == 1
  await session_mgr.persist_and_broadcast(node.id, dict(TOOL_RESULT_ECHO))
  await tree.dispatch.finish_run(node.id, run.id, outcome="success")
  assert tree.dispatch.pending_inputs(node.id) == []
  assert tree.dispatch.pending_input_blockers(node.id) == []
  # The echo stays in the history (it is evidence), just never as input.
  assert any(e.get("tool_use_result") for e in tree.events.load_events(node.id))


@pytest.mark.asyncio
async def test_stopped_round_is_handled_and_next_message_launches_at_once(
    tmp_path: Path) -> None:
  """A round stopped mid-run counts as handled: its batch never reappears as
  pending, and the next message dispatches a new round immediately whose batch
  holds only the new message (the probe_stop_then_message.py scenario)."""
  _, _session_mgr, tree = build_env(tmp_path)
  node = await create_task(tree, parent=None, request_id="node")
  m1 = await admit(tree, node.id, "long job")
  run = await tree.runs.register_run(
      RunRecord(id="run-live", session_id=node.id, kind="manager_turn"))
  await tree.dispatch.claim_input_batch(node.id, run.id)
  proc = live_subprocess()
  try:
    pid, pid_start = identity_of(proc.pid)
    await tree.runs.record_launch(node.id, run.id, pid=pid, pid_start=pid_start)
    stop = await tree.runs.request_stop(node.id, run.id, "stop-1")
    assert stop.stop_requested is True and stop.outcome == "interrupted"
  finally:
    if proc.poll() is None:
      proc.kill()
  m2 = await admit(tree, node.id, "new instruction after stop")
  assert [str(e["id"]) for e in tree.dispatch.pending_inputs(node.id)] == [str(m2["id"])]
  executor = ScriptedExecutor(tree)
  tree.dispatch.executor = executor
  decision = await tree.dispatch.dispatch_pending(node.id)
  assert decision["launch"] is True
  assert executor.batches == [(node.id, [str(m2["id"])])]
  # The stopped round's message is handled: it is never rerun and never
  # re-acknowledgable, and the new round's batch holds only the new message.
  with pytest.raises(TaskConflictError, match="not pending"):
    await tree.completion.acknowledge_inputs(
        node.id, request_id="ack-1", input_ids=[str(m1["id"])], note="drop", caller=OPERATOR)


@pytest.mark.asyncio
async def test_failed_and_interrupted_rounds_neither_block_nor_rerun(tmp_path: Path) -> None:
  """A failed round and an interrupted round (recovery marks a dead launched
  run interrupted) are both handled: neither blocks the next message, and
  neither batch reappears as pending."""
  _, _session_mgr, tree = build_env(tmp_path)
  node = await create_task(tree, parent=None, request_id="node")
  failed_in = await admit(tree, node.id, "do the thing")
  failed_run = await tree.runs.register_run(
      RunRecord(id="run-failed", session_id=node.id, kind="manager_turn"))
  await tree.dispatch.claim_input_batch(node.id, failed_run.id)
  await tree.dispatch.finish_run(node.id, failed_run.id, outcome="failed", exit_code=1)

  interrupted_in = await admit(tree, node.id, "then the other thing")
  interrupted_run = await tree.runs.register_run(
      RunRecord(id="run-interrupted", session_id=node.id, kind="manager_turn"))
  await tree.dispatch.claim_input_batch(node.id, interrupted_run.id)
  proc = live_subprocess()
  try:
    pid, pid_start = identity_of(proc.pid)
    await tree.runs.record_launch(node.id, interrupted_run.id, pid=pid, pid_start=pid_start)
  finally:
    if proc.poll() is None:
      proc.kill()
  # The dead process's terminal fact lands the way recovery records it.
  await tree.runs.record_finish(node.id, interrupted_run.id, "interrupted")

  next_in = await admit(tree, node.id, "carry on")
  assert [str(e["id"]) for e in tree.dispatch.pending_inputs(node.id)] == [str(next_in["id"])]
  executor = ScriptedExecutor(tree)
  tree.dispatch.executor = executor
  decision = await tree.dispatch.dispatch_pending(node.id)
  assert decision["launch"] is True
  assert executor.batches == [(node.id, [str(next_in["id"])])]
  assert {str(failed_in["id"]), str(interrupted_in["id"])}.isdisjoint(
      {str(e["id"]) for e in tree.dispatch.pending_inputs(node.id)})


@pytest.mark.asyncio
async def test_messages_arriving_during_a_round_form_one_next_round(tmp_path: Path) -> None:
  """Two inputs admitted while a round runs merge into the single next round's
  batch when the running round finishes."""
  _, _session_mgr, tree = build_env(tmp_path)
  node = await create_task(tree, parent=None, request_id="node")
  await admit(tree, node.id, "first instruction", input_id="m-1")
  executor = ScriptedExecutor(tree, finish=False)
  tree.dispatch.executor = executor
  await tree.dispatch.dispatch_pending(node.id)
  run_id = executor.tree.runs.list_run_records_sync(node.id)[0].id
  await admit(tree, node.id, "second instruction", input_id="m-2")
  await admit(tree, node.id, "third instruction", input_id="m-3")
  assert [str(e["id"]) for e in tree.dispatch.pending_inputs(node.id)] == ["m-2", "m-3"]

  executor.finish = True
  await tree.dispatch.finish_run(node.id, run_id, outcome="success")
  decision = await tree.dispatch.dispatch_pending(node.id)
  assert decision["launch"] is True
  assert executor.batches[-1][1] == ["m-2", "m-3"]


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


@pytest.mark.asyncio
async def test_input_identity_is_stable_and_attachments_preserved(tmp_path: Path) -> None:
  cfg, session_mgr, tree = build_env(tmp_path)
  node = await create_task(tree, parent=None, request_id="node")
  uploads = [{"filename": "notes.txt", "path": "/tmp/notes.txt"}]
  first = await admit(tree, node.id, "with attachment", input_id="stable-1", uploaded_files=uploads)
  replay = await admit(tree, node.id, "with attachment", input_id="stable-1", uploaded_files=uploads)
  assert first["id"] == replay["id"] == "stable-1"
  events = [e for e in tree.events.load_events(node.id) if e.get("id") == "stable-1"]
  assert len(events) == 1
  assert events[0]["uploaded_files"] == uploads
  assert events[0]["type"] == ET.USER and events[0]["actor"] == "user"

  # A dispatcher without an executor never launches and never acknowledges.
  fresh = TaskTreeManager(cfg, session_mgr)
  fresh.dispatch.executor = None
  decision = await fresh.dispatch.dispatch_pending(node.id)
  assert decision["launch"] is False and "next stage" in decision["reason"]
  assert [str(e["id"]) for e in input_events(fresh, node.id)] == ["stable-1"]


# ---------------------------------------------------------------------------
# Index races: phantom nodes and stale installs
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_blocked_publish_and_tree_query_show_no_phantom_node(tmp_path: Path) -> None:
  cfg, session_mgr, tree = build_env(tmp_path)
  await create_task(tree, parent=None, request_id="root")
  await tree._get_index()  # warm

  started = threading.Event()
  release = threading.Event()
  real_replace = os.replace

  def blocked_replace(src: str, dst: str) -> None:
    if os.path.basename(str(src)).startswith(".task-") and str(src).endswith(".tmp"):
      started.set()
      release.wait(10)
    real_replace(src, dst)

  import src.core.task_sessions as task_sessions_module
  original_replace = task_sessions_module.os.replace
  task_sessions_module.os.replace = blocked_replace  # type: ignore[assignment]

  async def do_create() -> None:
    await create_task(tree, parent=None, request_id="phantom")

  result: dict[str, object] = {}

  def run_create() -> None:
    result["task"] = asyncio.run(do_create())

  try:
    thread = threading.Thread(target=run_create)
    thread.start()
    assert started.wait(10), "the create never reached its publish step"
    # A concurrent tree query sees no phantom: the unpublished staging
    # directory (metadata.json already written) is never a node.
    query = TaskTreeManager(cfg, session_mgr)
    page = await query.tree_page(parent_id=None, include_archived=True, limit=100, cursor=None)
    assert len(page["items"]) == 1
    release.set()
    thread.join(30)
    assert not thread.is_alive()
  finally:
    task_sessions_module.os.replace = original_replace  # type: ignore[assignment]
    release.set()
  page = await tree.tree_page(parent_id=None, include_archived=True, limit=100, cursor=None)
  assert len(page["items"]) == 2  # the published node is now visible


@pytest.mark.asyncio
async def test_stale_index_build_never_installs_over_newer_writes(tmp_path: Path) -> None:
  _cfg, _session_mgr, tree = build_env(tmp_path)
  await create_task(tree, parent=None, request_id="root")
  await tree._get_index()

  tree._index = None  # TTL-expired state: the next query must build
  real_build = tree._build_index_sync

  def racing_build() -> object:
    index = real_build()
    # A structural write lands while this build is in flight.
    tree._invalidate_index()
    return index

  tree._build_index_sync = racing_build  # type: ignore[method-assign]
  stale = await tree._get_index()
  assert stale is not None
  assert tree._index is None  # the stale result was never installed
  tree._build_index_sync = real_build  # type: ignore[method-assign]

  await create_task(tree, parent=None, request_id="second")
  page = await tree.tree_page(parent_id=None, include_archived=True, limit=100, cursor=None)
  assert len(page["items"]) == 2  # the next query rebuilds and sees the write


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
      child.id, source_event={"id": "src-1"}, outcome="failed", summary="s",
      result_refs=[], recipient=other_root.id)
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
    await tree.delete_permanently(other_root.id, caller=CallerIdentity(
        kind="run", claims=RunTokenClaims(run_id="r", session_id=other_root.id, agent="a")))


# ---------------------------------------------------------------------------
# Real routes: browser and agent-relay input through the dispatcher
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_message_routes_use_the_dispatcher_on_v2_nodes(tmp_path: Path) -> None:
  from fastapi import FastAPI
  from fastapi.testclient import TestClient

  import src.api.chat as chat_api
  import src.api.internal as internal_api
  import src.api.sessions as sessions_api
  from src.api.deps import get_config, get_run_store, get_session_manager, get_task_manager

  cfg, session_mgr, tree = build_env(tmp_path)
  root = await create_task(tree, parent=None, request_id="root", name="Root")
  worker = await create_task(tree, parent=root.id, request_id="worker", profile="worker")

  app = FastAPI()
  app.include_router(sessions_api.router, prefix="/api/sessions")
  app.include_router(chat_api.router, prefix="/api/sessions")
  app.include_router(internal_api.router, prefix="/api/internal")
  key = "op-secret"
  stub_credentials({"charliebot": {"access_key": key}})
  app.dependency_overrides[get_config] = lambda: cfg
  app.dependency_overrides[get_session_manager] = lambda: session_mgr
  app.dependency_overrides[get_task_manager] = lambda: tree
  app.dependency_overrides[get_run_store] = lambda: tree.runs

  class _StreamingManager:
    def __init__(self) -> None:
      self.sent: list[tuple[str, dict]] = []

    async def broadcast(self, channel: str, payload: dict) -> None:
      self.sent.append((channel, payload))

  import src.core.sessions as sessions_module
  original_streaming = sessions_module.streaming_manager
  fake_streaming = _StreamingManager()
  sessions_module.streaming_manager = fake_streaming  # type: ignore[assignment]
  try:
    with TestClient(app) as client:
      # Operator browser input is a real USER event with its attachments.
      sent = client.post(f"/api/sessions/{worker.id}/message", json={
          "content": "please handle this",
          "uploaded_files": [{"filename": "brief.txt", "path": "/tmp/brief.txt"}],
      })
      assert sent.status_code == 202
      events = tree.events.load_events(worker.id)
      users = [e for e in events if e["type"] == ET.USER]
      assert len(users) == 1
      assert users[0]["content"] == "please handle this"
      assert users[0]["uploaded_files"][0]["filename"] == "brief.txt"
      assert users[0]["actor"] == "user"

      # A run-token agent on the same user-message route stays agent input
      # with its own session's provenance: never a real USER event.
      await tree.runs.register_run(RunRecord(id="run-agent", session_id=worker.id, kind="work"))
      # The run credential is accepted only once the launch callback persisted
      # the process identity on the Run.
      await tree.runs.record_launch(worker.id, "run-agent", pid=424242, pid_start="ps-1")
      claims = RunTokenClaims(run_id="run-agent", session_id=worker.id, agent="worker-agent")
      agent_client_headers = {"Authorization": f"Bearer {sign_run_token(claims, key)}"}
      relayed = client.post(f"/api/sessions/{worker.id}/message", json={"content": "agent view"},
                            headers=agent_client_headers)
      assert relayed.status_code == 202
      events = tree.events.load_events(worker.id)
      assert len([e for e in events if e["type"] == ET.USER]) == 1  # no second USER
      agents = [e for e in events if e["type"] == ET.AGENT_MESSAGE]
      assert agents and agents[0]["from_session"] == worker.id

      # The agent-relay API persists the agent message durably with provenance.
      relay_api = client.post("/api/internal/session-message", json={
          "session_id": root.id, "target_session_id": worker.id, "content": "from the manager",
      })
      assert relay_api.status_code == 200
      events = tree.events.load_events(worker.id)
      agents = [e for e in events if e["type"] == ET.AGENT_MESSAGE]
      assert agents[-1]["content"] == "from the manager"
      assert agents[-1]["from_session"] == root.id
      assert agents[-1]["from_session_name"] == "Root"

      # Both paths announce through the live wire after the durable append.
      message_deltas = [p for _channel, p in fake_streaming.sent if p.get("type") == "message"]
      roles = [d["message"]["role"] for d in message_deltas]
      assert roles.count("user") == 1 and roles.count("agent_message") == 2
  finally:
    sessions_module.streaming_manager = original_streaming  # type: ignore[assignment]


@pytest.mark.asyncio
async def test_complete_cancel_reopen_routes_and_scope(tmp_path: Path) -> None:
  from fastapi import FastAPI
  from fastapi.testclient import TestClient

  import src.api.sessions as sessions_api
  from src.api.deps import get_config, get_run_store, get_session_manager, get_task_manager

  cfg, session_mgr, tree = build_env(tmp_path)
  root = await create_task(tree, parent=None, request_id="root")
  worker = await create_task(tree, parent=root.id, request_id="worker", profile="worker")

  app = FastAPI()
  app.include_router(sessions_api.router, prefix="/api/sessions")
  key = "op-secret"
  stub_credentials({"charliebot": {"access_key": key}})
  app.dependency_overrides[get_config] = lambda: cfg
  app.dependency_overrides[get_session_manager] = lambda: session_mgr
  app.dependency_overrides[get_task_manager] = lambda: tree
  app.dependency_overrides[get_run_store] = lambda: tree.runs
  with TestClient(app) as client:
    # Blockers are a concrete 409 list: the open child blocks the root.
    blocked = client.post(f"/api/sessions/{root.id}/complete", json={
        "request_id": "route-1", "summary": "s", "result_refs": [], "run_ids": []})
    assert blocked.status_code == 409
    assert any("open descendant" in b for b in blocked.json()["detail"]["blockers"])

    # The worker's successful run closes it; the route returns the Session detail.
    await tree.runs.register_run(RunRecord(id="run-w", session_id=worker.id, kind="work"))
    await tree.dispatch.finish_run(worker.id, "run-w", outcome="success")
    ok = client.post(f"/api/sessions/{root.id}/complete", json={
        "request_id": "route-2", "summary": "delivered",
        "result_refs": ["run:run-w"], "run_ids": ["run-w"]})
    assert ok.status_code == 409  # the worker's report is unprocessed input
    await tree.runs.register_run(
        RunRecord(id="run-root-turn", session_id=root.id, kind="manager_turn"))
    await tree.dispatch.claim_input_batch(root.id, "run-root-turn")
    await tree.dispatch.finish_run(root.id, "run-root-turn", outcome="success")
    ok = client.post(f"/api/sessions/{root.id}/complete", json={
        "request_id": "route-2", "summary": "delivered",
        "result_refs": ["run:run-root-turn"], "run_ids": ["run-root-turn"]})
    assert ok.status_code == 200 and ok.json()["task_state"] == "completed"

    # Duplicate operation id replays the original outcome.
    replay = client.post(f"/api/sessions/{root.id}/complete", json={
        "request_id": "route-2", "summary": "delivered",
        "result_refs": ["run:run-root-turn"], "run_ids": ["run-root-turn"]})
    assert replay.status_code == 200
    closes = [e for e in tree.events.load_events(root.id) if e["type"] == ET.TASK_CLOSED]
    assert len(closes) == 1

    # Reopen is operator action; an agent token cannot mutate.
    reopened = client.post(f"/api/sessions/{root.id}/reopen", json={
        "request_id": "route-3", "reason": "more work"})
    assert reopened.status_code == 200 and reopened.json()["task_state"] == "open"
    await tree.runs.register_run(RunRecord(id="run-root-agent", session_id=root.id, kind="work"))
    await tree.runs.record_launch(root.id, "run-root-agent", pid=424243, pid_start="ps-2")
    key2_headers = {"Authorization": f"Bearer {sign_run_token(
        RunTokenClaims(run_id='run-root-agent', session_id=root.id, agent='a'), key)}"}
    forbidden = client.post(f"/api/sessions/{root.id}/reopen", json={
        "request_id": "route-4", "reason": "x"}, headers=key2_headers)
    assert forbidden.status_code == 403
    await tree.dispatch.finish_run(root.id, "run-root-agent", outcome="success")

    # Cancel preserves history and stays visible.
    cancelled = client.post(f"/api/sessions/{root.id}/cancel", json={
        "request_id": "route-5", "reason": "not needed"})
    assert cancelled.status_code == 200 and cancelled.json()["task_state"] == "cancelled"
    index = await tree._get_index()
    assert tree.archived_of(index, index.metas[root.id]) is False
    # Cancel is refused on a task that is not open.
    refused = client.post(f"/api/sessions/{root.id}/cancel", json={
        "request_id": "route-6", "reason": "again"})
    assert refused.status_code == 409


@pytest.mark.asyncio
async def test_batchless_finish_never_acknowledges_another_runs_claimed_batch(tmp_path: Path) -> None:
  """The locked finish layer itself rejects it: a batchless run's finisher
  cannot name ids another registered non-terminal run claimed (a claim that
  lands between the dispatcher's pre-check and the locked finish still fails)."""
  from src.core.runs import RunInputMismatchError

  _cfg, _session_mgr, tree = build_env(tmp_path)
  task = await create_task(tree, parent=None, request_id="root")
  await admit(tree, task.id, "work", input_id="in-1")
  await tree.runs.register_run(RunRecord(id="run-b", session_id=task.id, kind="work"))
  claimed = await tree.dispatch.claim_input_batch(task.id, "run-b")
  assert claimed == ["in-1"]
  await tree.runs.register_run(RunRecord(id="run-a", session_id=task.id, kind="manager_turn"))
  with pytest.raises(RunInputMismatchError, match="claimed batch"):
    await tree.runs.record_finish(task.id, "run-a", "success", input_event_ids=["in-1"])
  # No terminal fact landed for run-a; the input stays claimed by its owner.
  assert tree.runs.terminal_outcome(tree.runs.load_events_sync(task.id), "run-a") is None
  assert input_events(tree, task.id) == []
  # The real owner's success acknowledges exactly its batch.
  await tree.dispatch.finish_run(task.id, "run-b", outcome="success")
  finished = [e for e in tree.events.load_events(task.id) if e["type"] == ET.RUN_FINISHED]
  assert finished and finished[-1]["run_id"] == "run-b"
  assert list(finished[-1]["input_event_ids"]) == ["in-1"]
