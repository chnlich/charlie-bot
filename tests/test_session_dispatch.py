"""Dispatch-stage tests: durable input, claims, parent reports, recovery over real on-disk state."""

from __future__ import annotations

from datetime import timedelta
from pathlib import Path

import pytest
from conftest import OPERATOR, OPUS_BACKEND_ID, build_env, create_scheduled_node, create_task, stub_credentials

from src.api.message_utils import events_to_view
from src.core import event_types as ET
from src.core.models import LastRunStatus, RunRecord, SessionStatus, ensure_utc, utc_now_iso
from src.core.run_token import CallerIdentity, RunTokenClaims, sign_run_token
from src.core.sessions import SessionManager
from src.core.task_sessions import (
    TaskConflictError,
    TaskForbiddenError,
    TaskTreeManager,
)


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
async def test_recovery_redelivery_of_a_fresh_report_wakes_the_parent(tmp_path: Path) -> None:
  """The crash-window repair's freshly delivered report is the parent's new
  durable input: the delivery entry wakes the parent (its next serialized turn
  dispatches now, without waiting for the next message or restart). A replayed
  recovery pass re-derives the same report id, delivers nothing, wakes nobody."""
  _, _, tree = build_env(tmp_path)
  parent = await create_task(tree, parent=None, request_id="parent")
  child = await create_task(tree, parent=parent.id, request_id="a", profile="worker")

  # The close fact exists with an owed report, delivered by nobody: the crash
  # window recover_pending_reports repairs.
  close_event = {
      "id": "close-child",
      "type": ET.TASK_CLOSED,
      "timestamp": utc_now_iso(),
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
  assert [e for e in tree.events.load_events(parent.id) if e.get("type") == ET.CHILD_REPORT] == []
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
async def test_delivery_crash_windows_repair_after_a_fresh_instance(tmp_path: Path) -> None:
  cfg, session_mgr, tree = build_env(tmp_path)
  parent = await create_task(tree, parent=None, request_id="parent")
  child = await create_task(tree, parent=parent.id, request_id="child", profile="worker")

  # Window (a): the child's close fact landed but the parent append was lost.
  close_event = {
      "id": "close-child",
      "type": ET.TASK_CLOSED,
      "timestamp": utc_now_iso(),
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
  redelivered, created = await fresh.dispatch.deliver_child_report(
      child.id, source_event=close_event, outcome="completed", summary="delivered", result_refs=[], recipient=parent.id)
  assert not created and redelivered["id"] == delivered[0]["id"]
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
# Closed nodes, authorization identities, stopped queued runs
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_closed_node_keeps_input_and_agent_content_never_mints_authorization(tmp_path: Path) -> None:
  _, _, tree = build_env(tmp_path)
  root = await create_task(tree, parent=None, request_id="root")
  worker = await create_task(tree, parent=root.id, request_id="worker", profile="worker")

  # Agent and cron input carry their own types even with takeoff in the text;
  # they never mint a user authorization window. The agent message is refused
  # at request entry (the sender holds no implementation authorization), so
  # only the server-owned cron trigger is enqueued — whose takeoff text still
  # mints nothing.
  with pytest.raises(TaskForbiddenError):
    await admit(tree, worker.id, "take off now", event_type=ET.AGENT_MESSAGE, actor="agent", from_session=root.id)
  await admit(tree, worker.id, "take off (cron)", event_type=ET.SCHEDULED_TRIGGER, actor="system")
  from src.core.takeoff_gate import DelegationBlockedError
  with pytest.raises(DelegationBlockedError):
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

  # The closed node keeps late input as history.
  await admit(tree, worker.id, "late arrival", input_id="late-1")
  decision = await tree.dispatch.dispatch_pending(worker.id)
  assert decision["launch"] is False and "closed" in decision["reason"]
  assert [str(e["id"]) for e in input_events(tree, worker.id)] == ["late-1"]

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
async def test_legacy_trigger_wake_is_refused_at_v2_nodes(tmp_path: Path) -> None:
  """A scheduled/iteration wake aimed at a v2 node is explicitly unavailable (next stage):
  nothing is written through the legacy user-event path and nothing dispatches."""
  cfg, session_mgr, tree = build_env(tmp_path)
  manager = await create_task(tree, parent=None, request_id="root", name="Root")

  executor = ScriptedExecutor(tree)
  tree.dispatch.executor = executor
  from src.core.master_trigger import trigger_master
  await trigger_master(manager.id, "a worker result landed", cfg, session_mgr, ET.CHILD_REPORT)

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
    await trigger_master(v1.id, "a worker result landed", cfg, session_mgr, ET.CHILD_REPORT)
  finally:
    master_trigger_module.run_message = original
  assert called == [True]


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
      sent = client.post(
          f"/api/sessions/{worker.id}/message",
          json={
              "content": "please handle this",
              "uploaded_files": [{
                  "filename": "brief.txt",
                  "path": "/tmp/brief.txt"
              }],
          })
      assert sent.status_code == 202
      events = tree.events.load_events(worker.id)
      users = [e for e in events if e["type"] == ET.USER]
      assert len(users) == 1
      assert users[0]["content"] == "please handle this"
      assert users[0]["uploaded_files"][0]["filename"] == "brief.txt"
      assert users[0]["actor"] == "user"

      # A run-token agent on the same user-message route stays agent input
      # with its own session's provenance: never a real USER event. The
      # request-entry gate refuses it anyway — the worker is not its own
      # parent, so its self-addressed agent message is a 403 and nothing is
      # enqueued.
      await tree.runs.register_run(RunRecord(id="run-agent", session_id=worker.id, kind="work"))
      # The run credential is accepted only once the launch callback persisted
      # the process identity on the Run.
      await tree.runs.record_launch(worker.id, "run-agent", pid=424242, pid_start="ps-1")
      claims = RunTokenClaims(run_id="run-agent", session_id=worker.id, agent="worker-agent")
      agent_client_headers = {"Authorization": f"Bearer {sign_run_token(claims, key)}"}
      relayed = client.post(
          f"/api/sessions/{worker.id}/message", json={"content": "agent view"}, headers=agent_client_headers)
      assert relayed.status_code == 403
      events = tree.events.load_events(worker.id)
      assert len([e for e in events if e["type"] == ET.USER]) == 1  # no second USER
      assert not [e for e in events if e["type"] == ET.AGENT_MESSAGE]

      # The agent-relay API delivers an agent message to the worker only from
      # its authorized parent: with no user authorization anywhere the gate
      # refuses the manager's instruction with 403 and nothing is enqueued.
      relay_api = client.post(
          "/api/internal/session-message",
          json={
              "session_id": root.id,
              "target_session_id": worker.id,
              "content": "from the manager",
          })
      assert relay_api.status_code == 403
      assert "Delegation blocked" in relay_api.json()["detail"]
      events = tree.events.load_events(worker.id)
      assert not [e for e in events if e["type"] == ET.AGENT_MESSAGE]

      # Only the operator's user message reached the live wire.
      message_deltas = [p for _channel, p in fake_streaming.sent if p.get("type") == "message"]
      roles = [d["message"]["role"] for d in message_deltas]
      assert roles.count("user") == 1 and roles.count("agent_message") == 0
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
    blocked = client.post(
        f"/api/sessions/{root.id}/complete",
        json={
            "request_id": "route-1",
            "summary": "s",
            "result_refs": [],
            "run_ids": []
        })
    assert blocked.status_code == 409
    assert any("open descendant" in b for b in blocked.json()["detail"]["blockers"])

    # The worker's successful run closes it; the route returns the Session detail.
    await tree.runs.register_run(RunRecord(id="run-w", session_id=worker.id, kind="work"))
    await tree.dispatch.finish_run(worker.id, "run-w", outcome="success")
    ok = client.post(
        f"/api/sessions/{root.id}/complete",
        json={
            "request_id": "route-2",
            "summary": "delivered",
            "result_refs": ["run:run-w"],
            "run_ids": ["run-w"]
        })
    assert ok.status_code == 409  # the worker's report is unprocessed input
    await tree.runs.register_run(RunRecord(id="run-root-turn", session_id=root.id, kind="manager_turn"))
    await tree.dispatch.claim_input_batch(root.id, "run-root-turn")
    await tree.dispatch.finish_run(root.id, "run-root-turn", outcome="success")
    ok = client.post(
        f"/api/sessions/{root.id}/complete",
        json={
            "request_id": "route-2",
            "summary": "delivered",
            "result_refs": ["run:run-root-turn"],
            "run_ids": ["run-root-turn"]
        })
    assert ok.status_code == 200 and ok.json()["task_state"] == "completed"

    # Duplicate operation id replays the original outcome.
    replay = client.post(
        f"/api/sessions/{root.id}/complete",
        json={
            "request_id": "route-2",
            "summary": "delivered",
            "result_refs": ["run:run-root-turn"],
            "run_ids": ["run-root-turn"]
        })
    assert replay.status_code == 200
    closes = [e for e in tree.events.load_events(root.id) if e["type"] == ET.TASK_CLOSED]
    assert len(closes) == 1

    # Reopen is operator action; an agent token cannot mutate.
    reopened = client.post(f"/api/sessions/{root.id}/reopen", json={"request_id": "route-3", "reason": "more work"})
    assert reopened.status_code == 200 and reopened.json()["task_state"] == "open"
    await tree.runs.register_run(RunRecord(id="run-root-agent", session_id=root.id, kind="work"))
    await tree.runs.record_launch(root.id, "run-root-agent", pid=424243, pid_start="ps-2")
    agent_token = sign_run_token(RunTokenClaims(run_id="run-root-agent", session_id=root.id, agent="a"), key)
    key2_headers = {"Authorization": f"Bearer {agent_token}"}
    forbidden = client.post(
        f"/api/sessions/{root.id}/reopen", json={
            "request_id": "route-4",
            "reason": "x"
        }, headers=key2_headers)
    assert forbidden.status_code == 403
    await tree.dispatch.finish_run(root.id, "run-root-agent", outcome="success")

    # Cancel preserves history and stays visible.
    cancelled = client.post(f"/api/sessions/{root.id}/cancel", json={"request_id": "route-5", "reason": "not needed"})
    assert cancelled.status_code == 200 and cancelled.json()["task_state"] == "cancelled"
    index = await tree._get_index()
    assert tree.archived_of(index, index.metas[root.id]) is False
    # Cancel is refused on a task that is not open.
    refused = client.post(f"/api/sessions/{root.id}/cancel", json={"request_id": "route-6", "reason": "again"})
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


# ---------------------------------------------------------------------------
# Sidebar order: the user's own actions lift updated_at
# ---------------------------------------------------------------------------


async def updated_at_of(session_mgr: SessionManager, session_id: str):
  """One node's current sidebar sort key."""
  meta = await session_mgr.get_session(session_id)
  assert meta is not None
  return meta.updated_at


@pytest.mark.asyncio
async def test_user_admission_lifts_the_branch_and_stops_at_an_archived_ancestor(tmp_path: Path) -> None:
  """A real user message lifts the target and its unarchived ancestors to the
  event's time; an archived ancestor and everything above it keep their
  updated_at; a replayed input_id lifts nothing; agent messages, child
  reports, and scheduled triggers never lift."""
  _, session_mgr, tree = build_env(tmp_path)
  root = await create_task(tree, parent=None, request_id="root", name="Root")
  mid = await create_task(tree, parent=root.id, request_id="mid", name="Mid")
  child = await create_task(tree, parent=mid.id, request_id="child", name="Child")
  first = (await updated_at_of(session_mgr, child.id) + timedelta(hours=1)).isoformat()
  second = (await updated_at_of(session_mgr, child.id) + timedelta(hours=2)).isoformat()

  await admit(tree, child.id, "my turn", input_id="m1", timestamp=first)
  assert await updated_at_of(session_mgr, child.id) == ensure_utc(first)
  assert await updated_at_of(session_mgr, mid.id) == ensure_utc(first)
  assert await updated_at_of(session_mgr, root.id) == ensure_utc(first)

  # A replayed input_id returns the original event and lifts nothing; a new
  # message moves the whole branch forward again.
  await admit(tree, child.id, "my turn", input_id="m1", timestamp=second)
  assert await updated_at_of(session_mgr, child.id) == ensure_utc(first)
  await admit(tree, child.id, "again", input_id="m2", timestamp=second)
  assert await updated_at_of(session_mgr, child.id) == ensure_utc(second)
  assert await updated_at_of(session_mgr, mid.id) == ensure_utc(second)
  assert await updated_at_of(session_mgr, root.id) == ensure_utc(second)

  # Agent relays, scheduled triggers, and child reports are server/agent
  # traffic: none of them lifts, however fresh.
  agent_at = (await updated_at_of(session_mgr, child.id) + timedelta(hours=3)).isoformat()
  await admit(tree, child.id, "relayed", event_type=ET.AGENT_MESSAGE, actor="agent", timestamp=agent_at)
  await admit(
      tree, child.id, "cron woke this node", event_type=ET.SCHEDULED_TRIGGER, actor="system", timestamp=agent_at)
  await admit(tree, child.id, "child finished", event_type=ET.CHILD_REPORT, actor="system", timestamp=agent_at)
  assert await updated_at_of(session_mgr, child.id) == ensure_utc(second)
  assert await updated_at_of(session_mgr, mid.id) == ensure_utc(second)
  assert await updated_at_of(session_mgr, root.id) == ensure_utc(second)

  # An archived ancestor stops the climb: the archived row and everything
  # above it keep their updated_at even though the message is fresher.
  aroot = await create_task(tree, parent=None, request_id="aroot", name="ARoot")
  amid = await create_task(tree, parent=aroot.id, request_id="amid", name="AMid")
  achild = await create_task(tree, parent=amid.id, request_id="achild", name="AChild")
  await session_mgr.archive_session(amid.id)
  archived_at = await updated_at_of(session_mgr, amid.id)
  root_at = await updated_at_of(session_mgr, aroot.id)
  later = (await updated_at_of(session_mgr, achild.id) + timedelta(hours=3)).isoformat()
  await admit(tree, achild.id, "to the archived branch", timestamp=later)
  assert await updated_at_of(session_mgr, achild.id) == ensure_utc(later)
  assert await updated_at_of(session_mgr, amid.id) == archived_at
  assert await updated_at_of(session_mgr, aroot.id) == root_at


@pytest.mark.asyncio
async def test_a_messaged_node_lists_ahead_of_a_fired_scheduled_node(tmp_path: Path) -> None:
  """Listing order follows the user's actions: a fire leaves the scheduled
  node's updated_at alone, so a node the user just messaged lists ahead of it
  even when it started out older."""
  _, session_mgr, tree = build_env(tmp_path)
  x = await create_task(tree, parent=None, request_id="x", name="X")
  s = await create_scheduled_node(tree, name="nightly", backend=OPUS_BACKEND_ID)
  # Pin both rows into the past, X two hours older than the scheduled node.
  s_meta = await session_mgr.get_session(s.id)
  assert s_meta is not None
  base = s_meta.updated_at
  await session_mgr.update_thinking_state(x.id, base - timedelta(hours=2))
  await session_mgr.update_thinking_state(s.id, base - timedelta(hours=1))

  await tree.record_scheduled_fire(s.id, last_scheduled_run=base.isoformat(), last_run_status=LastRunStatus.SKIPPED)
  listing = await session_mgr.list_sessions(status=SessionStatus.ACTIVE)
  assert [r.id for r in listing] == [s.id, x.id]
  assert next(r for r in listing if r.id == s.id).last_run_status == LastRunStatus.SKIPPED

  # The user messages the older node; the fired node keeps its place.
  messaged_at = (base - timedelta(minutes=30)).isoformat()
  await admit(tree, x.id, "my turn", timestamp=messaged_at)
  listing = await session_mgr.list_sessions(status=SessionStatus.ACTIVE)
  assert [r.id for r in listing] == [x.id, s.id]
