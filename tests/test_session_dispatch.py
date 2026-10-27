"""Dispatch-stage tests: durable input, claims, parent reports, recovery over real on-disk state."""

from __future__ import annotations

import asyncio
import datetime
import pathlib

import conftest
import pytest

from src.infra import event_types as ET
from src.infra import models
from src.runtime import control_events, run_token, task_errors, task_sessions
from src.runtime.api import message_utils


async def admit(
    tree: task_sessions.TaskTreeManager,
    session_id: str,
    content: str,
    *,
    event_type: str = ET.USER,
    actor: str = "user",
    input_id: str | None = None,
    request_id: str | None = None,
    **payload: object) -> dict:
  return await tree.dispatch.admit_input(
      session_id,
      event_type=event_type,
      content=content,
      actor=actor,
      input_id=input_id,
      request_id=request_id,
      **payload)


class ScriptedExecutor:
  """The deterministic executor seam: binds one queued Run per dispatch and
  finishes it with the scripted outcome (or leaves it queued when finish=False)."""

  def __init__(self, tree: task_sessions.TaskTreeManager, *, outcome: str = "success", finish: bool = True) -> None:
    self.tree = tree
    self.outcome = outcome
    self.finish = finish
    self.batches: list[tuple[str, list[str]]] = []
    self.counter = 0

  async def __call__(self, session_id: str, pending: list[dict]) -> str:
    self.counter += 1
    batch_ids = [str(e["id"]) for e in pending]
    self.batches.append((session_id, batch_ids))
    run = await self.tree.runs.register_run_locked(
        models.RunRecord(id=f"consumer-{self.counter}", session_id=session_id, kind="work"))
    async with self.tree.control_lock:
      bound = await self.tree.dispatch.claim_input_batch_locked(session_id, run.id)
    assert bound == batch_ids
    if self.finish:
      await self.tree.dispatch.finish_run(session_id, run.id, outcome=self.outcome)
    return run.id


def child_report_messages(session_blocks: conftest.SessionBlocks, session_id: str) -> list[dict]:
  events = session_blocks.events.load_chat_events_sync(session_id)
  view, _draft = message_utils.events_to_view(events)
  return [m for m in view if m.get("role") == ET.CHILD_REPORT]


def input_events(tree: task_sessions.TaskTreeManager, session_id: str) -> list[dict]:
  return tree.dispatch.pending_inputs(session_id)


@pytest.mark.asyncio
async def test_request_id_replay_returns_the_stored_event_and_conflicts_on_changed_content(
    tmp_path: pathlib.Path) -> None:
  _, _, tree = conftest.build_env(tmp_path)
  root = await conftest.create_task(tree, parent=None, request_id="root")
  request_id = "browser-send-1"

  stored = await admit(tree, root.id, "hello", request_id=request_id)
  replay = await admit(tree, root.id, "hello", request_id=request_id)

  assert stored["id"] == control_events.stable_request_event_id(root.id, request_id)
  assert stored["request_id"] == request_id
  assert replay == stored
  assert [e for e in tree.events.load_events(root.id) if e.get("type") == ET.USER] == [stored]

  expected = f"request_id {request_id} is already event {stored['id']} with a different type or content"
  with pytest.raises(task_errors.TaskConflictError, match=expected):
    await admit(tree, root.id, "changed", request_id=request_id)
  with pytest.raises(task_errors.TaskConflictError, match=expected):
    await admit(tree, root.id, "hello", event_type=ET.AGENT_MESSAGE, actor="agent", request_id=request_id)


@pytest.mark.asyncio
async def test_request_id_and_input_id_are_mutually_exclusive(tmp_path: pathlib.Path) -> None:
  _, _, tree = conftest.build_env(tmp_path)
  root = await conftest.create_task(tree, parent=None, request_id="root")

  with pytest.raises(task_errors.TaskInvalidError, match="mutually exclusive"):
    await tree.dispatch.admit_input(
        root.id,
        event_type=ET.USER,
        content="hello",
        actor="user",
        input_id="manual-id",
        request_id="browser-id",
    )


@pytest.mark.asyncio
async def test_concurrent_request_id_admission_checks_and_appends_under_one_lock(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  _, _, tree = conftest.build_env(tmp_path)
  root = await conftest.create_task(tree, parent=None, request_id="root")
  first_append_entered = asyncio.Event()
  second_primed = asyncio.Event()
  release_append = asyncio.Event()
  append_count = 0
  prime_count = 0
  lock_states: list[bool] = []
  original_append = tree.events.append
  original_fact_history = tree.fact_history
  original_prime = tree.session_events.prime_aggregator

  async def gated_append(session_id: str, event: dict) -> None:
    nonlocal append_count
    append_count += 1
    lock_states.append(tree.control_lock.locked())
    if append_count == 1:
      first_append_entered.set()
      await release_append.wait()
    await original_append(session_id, event)

  def recording_fact_history(session_id: str) -> list[dict]:
    lock_states.append(tree.control_lock.locked())
    return original_fact_history(session_id)

  async def signal_second_prime(session_id: str) -> int:
    nonlocal prime_count
    prime_count += 1
    if prime_count == 2:
      second_primed.set()
    return await original_prime(session_id)

  monkeypatch.setattr(tree.events, "append", gated_append)
  monkeypatch.setattr(tree, "fact_history", recording_fact_history)
  monkeypatch.setattr(tree.session_events, "prime_aggregator", signal_second_prime)

  first = asyncio.create_task(admit(tree, root.id, "same", request_id="race"))
  await first_append_entered.wait()
  second = asyncio.create_task(admit(tree, root.id, "same", request_id="race"))
  await second_primed.wait()
  release_append.set()
  first_event, second_event = await asyncio.gather(first, second)

  assert first_event == second_event
  assert append_count == 1
  assert lock_states and all(lock_states)
  assert [e for e in tree.events.load_events(root.id) if e.get("request_id") == "race"] == [first_event]


# ---------------------------------------------------------------------------
# Parent reports: eligibility, dedup, crash windows
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_two_reports_one_acknowledged_reload_leaves_only_the_other(tmp_path: pathlib.Path) -> None:
  cfg, session_blocks, tree = conftest.build_env(tmp_path)
  parent = await conftest.create_task(tree, parent=None, request_id="parent")
  child_a = await conftest.create_task(tree, parent=parent.id, request_id="a", profile="worker")
  child_b = await conftest.create_task(tree, parent=parent.id, request_id="b", profile="worker")

  # Child A closes and reports; the parent's first consumer claims exactly that
  # report and finishes successfully, acknowledging only it.
  run_a = await tree.runs.register_run_locked(models.RunRecord(id="run-a", session_id=child_a.id, kind="work"))
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
  run_b = await tree.runs.register_run_locked(models.RunRecord(id="run-b", session_id=child_b.id, kind="work"))
  await tree.dispatch.finish_run(child_b.id, run_b.id, outcome="success")
  pending_now = {e["child_session_id"] for e in input_events(tree, parent.id)}
  assert pending_now == {child_b.id}

  # A fresh instance derives the same eligibility from disk facts alone.
  fresh = conftest.build_task_tree(cfg, session_blocks)
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
  assert len(child_report_messages(session_blocks, parent.id)) == 2


@pytest.mark.asyncio
async def test_recovery_redelivery_of_a_fresh_report_wakes_the_parent(tmp_path: pathlib.Path) -> None:
  """The crash-window repair's freshly delivered report is the parent's new
  durable input: the delivery entry wakes the parent (its next serialized turn
  dispatches now, without waiting for the next message or restart). A replayed
  recovery pass re-derives the same report id, delivers nothing, wakes nobody."""
  _, _, tree = conftest.build_env(tmp_path)
  parent = await conftest.create_task(tree, parent=None, request_id="parent")
  child = await conftest.create_task(tree, parent=parent.id, request_id="a", profile="worker")

  # The close fact exists with an owed report, delivered by nobody: the crash
  # window recover_pending_reports repairs.
  close_event = {
      "id": "close-child",
      "type": ET.TASK_CLOSED,
      "timestamp": models.utc_now_iso(),
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
  assert conftest.child_reports(tree, parent.id) == []
  assert tree.dispatch.undelivered_report_blockers(child.id), "the close fact must owe its report"

  wakes: list[str] = []
  real_dispatch = tree.dispatch.dispatch_pending

  async def counting_dispatch(session_id: str) -> dict:
    if session_id == parent.id:
      wakes.append(session_id)
    return await real_dispatch(session_id)

  tree.dispatch.dispatch_pending = counting_dispatch  # type: ignore[method-assign]

  delivered = await tree.dispatch.recover_pending_reports(child.id)
  assert len(delivered) == 1 and delivered[0]["child_session_id"] == child.id
  # The fresh report woke the parent exactly once.
  assert wakes == [parent.id]

  # A repeated recovery pass re-derives the same report id: no delivery, no wake.
  again = await tree.dispatch.recover_pending_reports(child.id)
  assert again == []
  assert wakes == [parent.id]


@pytest.mark.asyncio
async def test_delivery_crash_windows_repair_after_a_fresh_instance(tmp_path: pathlib.Path) -> None:
  cfg, session_blocks, tree = conftest.build_env(tmp_path)
  parent = await conftest.create_task(tree, parent=None, request_id="parent")
  child = await conftest.create_task(tree, parent=parent.id, request_id="child", profile="worker")

  # Window (a): the child's close fact landed but the parent append was lost.
  close_event = {
      "id": "close-child",
      "type": ET.TASK_CLOSED,
      "timestamp": models.utc_now_iso(),
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
  fresh = conftest.build_task_tree(cfg, session_blocks)

  # While the report is undelivered, the moving-subtree reparent guard blocks.
  other_root = await fresh.create_task(
      request_id="other-root",
      task_parent_id=None,
      profile="manager",
      task=None,
      name=None,
      backend=None,
      caller=conftest.OPERATOR)
  with pytest.raises(task_errors.TaskConflictError, match="undelivered parent report"):
    await fresh.patch_task(
        child.id, models.PatchSessionTaskRequest(task_parent_id=other_root.id), caller=conftest.OPERATOR)

  delivered = await fresh.dispatch.recover_pending_reports(child.id)
  assert len(delivered) == 1 and delivered[0]["child_session_id"] == child.id
  assert len(child_report_messages(session_blocks, parent.id)) == 1

  # A repeat recovery pass and a direct re-delivery both dedup to the same event.
  again = await fresh.dispatch.recover_pending_reports(child.id)
  assert again == []
  redelivered, created = await fresh.dispatch.deliver_child_report(
      child.id, source_event=close_event, outcome="completed", summary="delivered", result_refs=[], recipient=parent.id)
  assert not created and redelivered["id"] == delivered[0]["id"]
  assert len(child_report_messages(session_blocks, parent.id)) == 1

  # Window (b): the parent append landed but the enqueue was lost — the report
  # is a pending input the fresh instance's recovery calculation rediscovers.
  pending = input_events(fresh, parent.id)
  assert [str(e["id"]) for e in pending] == [delivered[0]["id"]]

  # With the report delivered, the same move passes its report guards.
  await fresh.patch_task(
      child.id, models.PatchSessionTaskRequest(task_parent_id=other_root.id), caller=conftest.OPERATOR)
  moved = await fresh.load_meta(child.id)
  assert moved is not None and moved.task_parent_id == other_root.id


# ---------------------------------------------------------------------------
# Closed nodes, authorization identities, stopped queued runs
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_closed_node_keeps_input_and_agent_content_never_mints_authorization(tmp_path: pathlib.Path) -> None:
  _, _, tree = conftest.build_env(tmp_path)
  root = await conftest.create_task(tree, parent=None, request_id="root")
  worker = await conftest.create_task(tree, parent=root.id, request_id="worker", profile="worker")

  # Agent and cron input carry their own types even with takeoff in the text;
  # the runtime gate never counts them as user messages. The agent message is
  # refused at request entry (the sender holds no implementation
  # authorization), so only the server-owned cron trigger is enqueued — and it
  # counts for nothing either.
  with pytest.raises(task_errors.TaskForbiddenError):
    await admit(tree, worker.id, "take off now", event_type=ET.AGENT_MESSAGE, actor="agent", from_session=root.id)
  await admit(tree, worker.id, "take off (cron)", event_type=ET.SCHEDULED_TRIGGER, actor="system")
  from src.runtime import takeoff_gate
  with pytest.raises(takeoff_gate.DelegationBlockedError):
    await tree.check_task_authorization(worker.id)

  # A consumer takes the cron input; with the node's inputs handled the
  # successful run auto-closes the worker.
  executor = ScriptedExecutor(tree)
  tree.dispatch.executor = executor
  await tree.dispatch.dispatch_pending(worker.id)
  assert len(executor.batches) == 2 and len(executor.batches[0][1]) == 1
  # The delivered close report is the parent's durable input: the close itself
  # dispatched the parent's next serialized turn through the same executor.
  assert executor.batches[1][0] == root.id and len(executor.batches[1][1]) == 1
  assert tree.task_state(worker.id) == "completed"  # auto-close landed

  # The closed node keeps a late machine input as history.
  with pytest.raises(task_errors.TaskArchivedError):
    await admit(
        tree, worker.id, "late machine arrival", event_type=ET.AGENT_MESSAGE, actor="agent", from_session=root.id)
  assert [str(e["id"]) for e in input_events(tree, worker.id)] == []
  decision = await tree.dispatch.dispatch_pending(worker.id)
  assert decision["launch"] is False and "closed" in decision["reason"]

  # A later authorized user retry restores the node and is the round's only
  # input: the restore's fresh boundary keeps the closed period as history.
  await admit(tree, worker.id, "take off — redo it", input_id="user-retry-1")
  reopens = [e for e in tree.events.load_events(worker.id) if e["type"] == ET.TASK_REOPENED]
  assert [e["reason"] for e in reopens] == ["user message"]
  assert tree.task_state(worker.id) == "open"
  assert [str(e["id"]) for e in input_events(tree, worker.id)] == ["user-retry-1"]


@pytest.mark.asyncio
async def test_stopped_queued_run_is_never_launched_and_releases_its_batch(tmp_path: pathlib.Path) -> None:
  _, _, tree = conftest.build_env(tmp_path)
  node = await conftest.create_task(tree, parent=None, request_id="node")
  await admit(tree, node.id, "work to do", input_id="job-1")
  queued = await tree.runs.register_run_locked(models.RunRecord(id="run-queued", session_id=node.id))
  async with tree.control_lock:
    await tree.dispatch.claim_input_batch_locked(node.id, queued.id)
  stop = await tree.runs.request_stop(node.id, "run-queued", "stop-1")
  assert stop.stop_requested is True and stop.outcome is None  # no exit was observed

  executor = ScriptedExecutor(tree)
  tree.dispatch.executor = executor
  decision = await tree.dispatch.dispatch_pending(node.id)
  assert decision["launch"] is True  # the stopped queued run never consumes
  assert executor.batches and executor.batches[0][1] == ["job-1"]

  with pytest.raises(task_errors.TaskConflictError, match="stop request"):
    async with tree.control_lock:
      await tree.dispatch.claim_input_batch_locked(node.id, "run-queued")


# ---------------------------------------------------------------------------
# Input candidacy and handling: tool echoes, stopped/failed/interrupted rounds
# ---------------------------------------------------------------------------

# ---------------------------------------------------------------------------
# Permanent delete: every reference category, one locked operation
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_deletion_rejects_each_reference_category_and_deletes_the_empty(tmp_path: pathlib.Path) -> None:
  _cfg, session_blocks, tree = conftest.build_env(tmp_path)
  root = await conftest.create_task(tree, parent=None, request_id="root")
  child = await conftest.create_task(tree, parent=root.id, request_id="child", profile="worker")

  # Child reference.
  with pytest.raises(task_errors.TaskConflictError, match="child task"):
    await tree.delete_permanently(root.id, caller=conftest.OPERATOR)
  # Runs.
  await tree.runs.register_run_locked(models.RunRecord(id="run-blk", session_id=child.id))
  with pytest.raises(task_errors.TaskConflictError, match="run record"):
    await tree.delete_permanently(child.id, caller=conftest.OPERATOR)
  # Preserved conversation beyond the creation fact.
  await admit(tree, child.id, "history", input_id="hist-1")
  with pytest.raises(task_errors.TaskConflictError, match="preserved conversation"):
    await tree.delete_permanently(child.id, caller=conftest.OPERATOR)
  # A child report held in another task's log.
  other_root = await conftest.create_task(tree, parent=None, request_id="other-root")
  await tree.dispatch.deliver_child_report(
      child.id, source_event={"id": "src-1"}, outcome="failed", summary="s", result_refs=[], recipient=other_root.id)
  with pytest.raises(task_errors.TaskConflictError, match="child report"):
    await tree.delete_permanently(child.id, caller=conftest.OPERATOR)
  # An origin reference from another task's saved metadata.
  fork_meta = await session_blocks.store.get_session(other_root.id)
  assert fork_meta is not None
  fork_meta.origin_ref = models.EventRef(session_id=child.id, event_id=None)
  await session_blocks.store.save_metadata(fork_meta)
  with pytest.raises(task_errors.TaskConflictError, match="origin_ref"):
    await tree.delete_permanently(child.id, caller=conftest.OPERATOR)

  # A truly empty, unreferenced task is deletable under the same lock.
  empty = await conftest.create_task(tree, parent=None, request_id="empty")
  assert await tree.delete_permanently(empty.id, caller=conftest.OPERATOR) is True
  assert await session_blocks.store.get_session(empty.id) is None
  # Agents cannot delete.
  with pytest.raises(task_errors.TaskForbiddenError):
    await tree.delete_permanently(
        other_root.id,
        caller=run_token.CallerIdentity(
            kind="run", claims=run_token.RunTokenClaims(run_id="r", session_id=other_root.id, agent="a")))


# ---------------------------------------------------------------------------
# Real routes: browser and agent-relay input through the dispatcher
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_input_mode_on_a_non_user_input_is_refused(tmp_path: pathlib.Path) -> None:
  _cfg, _blocks, tree = conftest.build_env(tmp_path)
  root = await conftest.create_task(tree, parent=None, request_id="root", name="Root")

  with pytest.raises(task_errors.TaskInvalidError, match="input_mode"):
    await admit(
        tree, root.id, "relayed", event_type=ET.AGENT_MESSAGE, actor="agent", from_session="peer", input_mode="voice")

  assert not [e for e in tree.events.load_events(root.id) if e["type"] == ET.AGENT_MESSAGE]


@pytest.mark.asyncio
async def test_batchless_finish_never_acknowledges_another_runs_claimed_batch(tmp_path: pathlib.Path) -> None:
  """The locked finish layer itself rejects it: a batchless run's finisher
  cannot name ids another registered non-terminal run claimed (a claim that
  lands between the dispatcher's pre-check and the locked finish still fails)."""
  from src.runtime import runs

  _cfg, _session_blocks, tree = conftest.build_env(tmp_path)
  task = await conftest.create_task(tree, parent=None, request_id="root")
  await admit(tree, task.id, "work", input_id="in-1")
  await tree.runs.register_run_locked(models.RunRecord(id="run-b", session_id=task.id, kind="work"))
  async with tree.control_lock:
    claimed = await tree.dispatch.claim_input_batch_locked(task.id, "run-b")
  assert claimed == ["in-1"]
  await tree.runs.register_run_locked(models.RunRecord(id="run-a", session_id=task.id, kind="manager_turn"))
  with pytest.raises(runs.RunInputMismatchError, match="claimed batch"):
    await tree.runs.record_finish(task.id, "run-a", "success", input_event_ids=["in-1"])
  # No terminal fact landed for run-a; the input stays claimed by its owner.
  assert tree.runs.terminal_outcome(tree.runs.load_events_sync(task.id), "run-a") is None
  assert input_events(tree, task.id) == []
  # The real owner's success acknowledges exactly its batch.
  await tree.dispatch.finish_run(task.id, "run-b", outcome="success")
  finished = [e for e in tree.events.load_events(task.id) if e["type"] == ET.RUN_FINISHED]
  assert finished and finished[-1]["run_id"] == "run-b"
  assert list(finished[-1]["input_event_ids"]) == ["in-1"]


# ---------------------------------------------------------------------------
# Sidebar order: the user's own actions lift updated_at
# ---------------------------------------------------------------------------


async def updated_at_of(session_blocks: conftest.SessionBlocks, session_id: str):
  """One node's current sidebar sort key."""
  meta = await session_blocks.store.get_session(session_id)
  assert meta is not None
  return meta.updated_at


@pytest.mark.asyncio
async def test_user_admission_lifts_only_the_target_node(tmp_path: pathlib.Path) -> None:
  """A real user message lifts only the node it is sent to: that node's
  updated_at moves to the event's time, forward only, and its parent and
  ancestors keep their values. A user message to a root lifts the root alone.
  A message to an archived node reopens it and its archived ancestors and lifts
  the node alone. A replayed input_id lifts nothing; agent messages, child
  reports, and scheduled triggers never lift."""
  _, session_blocks, tree = conftest.build_env(tmp_path)
  root = await conftest.create_task(tree, parent=None, request_id="root", name="Root")
  mid = await conftest.create_task(tree, parent=root.id, request_id="mid", name="Mid")
  child = await conftest.create_task(tree, parent=mid.id, request_id="child", name="Child")
  root_at = await updated_at_of(session_blocks, root.id)
  mid_at = await updated_at_of(session_blocks, mid.id)
  child_at = await updated_at_of(session_blocks, child.id)
  first = (child_at + datetime.timedelta(hours=1)).isoformat()
  second = (child_at + datetime.timedelta(hours=2)).isoformat()

  await admit(tree, child.id, "my turn", input_id="m1", timestamp=first)
  assert await updated_at_of(session_blocks, child.id) == models.ensure_utc(first)
  assert await updated_at_of(session_blocks, mid.id) == mid_at
  assert await updated_at_of(session_blocks, root.id) == root_at

  # A replayed input_id returns the original event and lifts nothing; a new
  # message lifts the leaf again and still leaves its ancestors in place.
  await admit(tree, child.id, "my turn", input_id="m1", timestamp=second)
  assert await updated_at_of(session_blocks, child.id) == models.ensure_utc(first)
  await admit(tree, child.id, "again", input_id="m2", timestamp=second)
  assert await updated_at_of(session_blocks, child.id) == models.ensure_utc(second)
  assert await updated_at_of(session_blocks, mid.id) == mid_at
  assert await updated_at_of(session_blocks, root.id) == root_at

  # Agent relays, scheduled triggers, and child reports are server/agent
  # traffic: none of them lifts, however fresh.
  agent_at = (await updated_at_of(session_blocks, child.id) + datetime.timedelta(hours=3)).isoformat()
  await admit(tree, child.id, "relayed", event_type=ET.AGENT_MESSAGE, actor="agent", timestamp=agent_at)
  await admit(
      tree, child.id, "cron woke this node", event_type=ET.SCHEDULED_TRIGGER, actor="system", timestamp=agent_at)
  await admit(tree, child.id, "child finished", event_type=ET.CHILD_REPORT, actor="system", timestamp=agent_at)
  assert await updated_at_of(session_blocks, child.id) == models.ensure_utc(second)
  assert await updated_at_of(session_blocks, mid.id) == mid_at
  assert await updated_at_of(session_blocks, root.id) == root_at

  # A message to the root lifts the root and nothing below it.
  root_msg = (root_at + datetime.timedelta(hours=4)).isoformat()
  await admit(tree, root.id, "to the root", timestamp=root_msg)
  assert await updated_at_of(session_blocks, root.id) == models.ensure_utc(root_msg)
  assert await updated_at_of(session_blocks, mid.id) == mid_at
  assert await updated_at_of(session_blocks, child.id) == models.ensure_utc(second)

  # Archiving the middle node archives the leaf beneath it. A message to the
  # leaf reopens the archived ancestor and the leaf, and lifts only the leaf.
  aroot = await conftest.create_task(tree, parent=None, request_id="aroot", name="ARoot")
  amid = await conftest.create_task(tree, parent=aroot.id, request_id="amid", name="AMid")
  achild = await conftest.create_task(tree, parent=amid.id, request_id="achild", name="AChild")
  await tree.completion.archive_subtree(
      amid.id, request_id="archive-amid", summary="archived", result_refs=[], run_ids=[], caller=conftest.OPERATOR)
  assert tree.task_state(achild.id) == "archived"
  amid_at = await updated_at_of(session_blocks, amid.id)
  aroot_at = await updated_at_of(session_blocks, aroot.id)
  later = (await updated_at_of(session_blocks, achild.id) + datetime.timedelta(hours=3)).isoformat()
  await admit(tree, achild.id, "to the archived branch", timestamp=later)
  assert tree.task_state(achild.id) == "open"
  assert await updated_at_of(session_blocks, achild.id) == models.ensure_utc(later)
  assert await updated_at_of(session_blocks, amid.id) == amid_at
  assert await updated_at_of(session_blocks, aroot.id) == aroot_at


@pytest.mark.asyncio
async def test_a_user_message_lift_is_stored_before_its_tree_changed_broadcast(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The browser refetches the sidebar on a message's task_tree_changed broadcast,
  so the target's stored updated_at is already the message time when it fires."""
  _, session_blocks, tree = conftest.build_env(tmp_path)
  root = await conftest.create_task(tree, parent=None, request_id="root", name="Root")
  child = await conftest.create_task(tree, parent=root.id, request_id="child", name="Child")
  when = (await updated_at_of(session_blocks, child.id) + datetime.timedelta(hours=1)).isoformat()
  broadcasts: list[tuple[str, str | None, datetime.datetime]] = []

  async def record_broadcast(session_id: str, event_type: str | None) -> None:
    # Record only: the sink logs and swallows a broadcast failure, so an
    # assertion here would never fail the test.
    meta = await session_blocks.store.get_session(session_id)
    broadcasts.append((session_id, event_type, meta.updated_at))

  monkeypatch.setattr(session_blocks.events, "broadcast_task_tree_changed", record_broadcast)
  await admit(tree, child.id, "my turn", timestamp=when)
  assert broadcasts == [(child.id, ET.USER, models.ensure_utc(when))]


@pytest.mark.asyncio
async def test_a_messaged_child_lists_ahead_of_its_siblings_and_roots_keep_their_order(tmp_path: pathlib.Path) -> None:
  """A user message moves only the row it was sent to: in the listing the
  messaged child lists ahead of its sibling children, and the root rows keep
  their relative order."""
  _, session_blocks, tree = conftest.build_env(tmp_path)
  r1 = await conftest.create_task(tree, parent=None, request_id="r1", name="R1")
  r2 = await conftest.create_task(tree, parent=None, request_id="r2", name="R2")
  c1 = await conftest.create_task(tree, parent=r1.id, request_id="c1", name="C1")
  c2 = await conftest.create_task(tree, parent=r1.id, request_id="c2", name="C2")
  c3 = await conftest.create_task(tree, parent=r1.id, request_id="c3", name="C3")
  # Pin each sort key, oldest first, so the order comes from the test's times.
  base = datetime.datetime(2026, 10, 1, tzinfo=datetime.timezone.utc)
  for hours, node in enumerate([c1, c2, c3, r1, r2]):
    await session_blocks.anchors.update_thinking_state(node.id, base + datetime.timedelta(hours=hours))
  listing = await session_blocks.listing.list_sessions(status=models.SessionStatus.ACTIVE)
  assert [r.id for r in listing] == [r2.id, r1.id, c3.id, c2.id, c1.id]

  await admit(tree, c1.id, "my turn", timestamp=(base + datetime.timedelta(hours=10)).isoformat())
  listing = await session_blocks.listing.list_sessions(status=models.SessionStatus.ACTIVE)
  assert [r.id for r in listing] == [c1.id, r2.id, r1.id, c3.id, c2.id]


@pytest.mark.asyncio
async def test_a_messaged_node_lists_ahead_of_a_fired_scheduled_node(tmp_path: pathlib.Path) -> None:
  """Listing order follows the user's actions: a fire leaves the scheduled
  node's updated_at alone, so a node the user just messaged lists ahead of it
  even when it started out older."""
  _, session_blocks, tree = conftest.build_env(tmp_path)
  x = await conftest.create_task(tree, parent=None, request_id="x", name="X")
  s = await conftest.create_scheduled_node(tree, name="nightly", backend=conftest.OPUS_BACKEND_ID)
  # Pin both rows into the past, X two hours older than the scheduled node.
  s_meta = await session_blocks.store.get_session(s.id)
  assert s_meta is not None
  base = s_meta.updated_at
  await session_blocks.anchors.update_thinking_state(x.id, base - datetime.timedelta(hours=2))
  await session_blocks.anchors.update_thinking_state(s.id, base - datetime.timedelta(hours=1))

  await tree.update_slot_fields(
      s.id, "cron", last_scheduled_run=base.isoformat(), last_run_status=models.LastRunStatus.SKIPPED)
  listing = await session_blocks.listing.list_sessions(status=models.SessionStatus.ACTIVE)
  assert [r.id for r in listing] == [s.id, x.id]
  assert next(r for r in listing if r.id == s.id).last_run_status == models.LastRunStatus.SKIPPED

  # The user messages the older node; the fired node keeps its place.
  messaged_at = (base - datetime.timedelta(minutes=30)).isoformat()
  await admit(tree, x.id, "my turn", timestamp=messaged_at)
  listing = await session_blocks.listing.list_sessions(status=models.SessionStatus.ACTIVE)
  assert [r.id for r in listing] == [x.id, s.id]
