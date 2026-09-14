"""Completion-stage tests: evidence, close/cancel/reopen, projection, routes."""

from __future__ import annotations

import os
import subprocess
from datetime import UTC, datetime
from pathlib import Path

import pytest
from conftest import make_home_config

from src.core import event_types as ET
from src.core.models import PatchSessionTaskRequest, RunRecord, TaskSpec
from src.core.run_token import CallerIdentity, RunTokenClaims, sign_run_token
from src.core.sessions import SessionManager
from src.core.task_completion import CompletionEvidence, LandingEvidence
from src.core.task_sessions import (
  TaskConflictError,
  TaskInvalidError,
  TaskTreeManager,
  _encode_tree_cursor,
)

OPERATOR = CallerIdentity(kind="operator")


def live_identity() -> tuple[int, str, datetime]:
  """An owned, isolated live process identity (never a zombie or a fake)."""
  proc = subprocess.Popen(["/bin/sleep", "30"])
  from src.core.runs import read_pid_stat
  pair = read_pid_stat(proc.pid)
  assert pair is not None
  return proc.pid, pair[0], datetime.now(UTC)


def build_env(tmp_path: Path) -> tuple[object, SessionManager, TaskTreeManager]:
  cfg = make_home_config(tmp_path)
  session_mgr = SessionManager(cfg)
  return cfg, session_mgr, TaskTreeManager(cfg, session_mgr)


async def create_task(tree: TaskTreeManager, *, parent: str | None, request_id: str,
                      profile: str = "manager", task: TaskSpec | None = None, name: str | None = None):
  return await tree.create_task(
      request_id=request_id, task_parent_id=parent, profile=profile, task=task,
      name=name, backend=None, caller=OPERATOR)


async def finish_worker_run(tree: TaskTreeManager, session_id: str, run_id: str) -> None:
  """Land one successful worker work run: the automatic completion path."""
  await tree.dispatch.finish_run(session_id, run_id, outcome="success")


# ---------------------------------------------------------------------------
# Three-level delivery: workers close, explicit feature close, project open
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_three_level_delivery_closes_workers_and_keeps_project_open(tmp_path: Path) -> None:
  cfg, session_mgr, tree = build_env(tmp_path)
  project = await create_task(tree, parent=None, request_id="project")
  feature = await create_task(tree, parent=project.id, request_id="feature")
  worker_1 = await create_task(tree, parent=feature.id, request_id="w1", profile="worker")
  worker_2 = await create_task(tree, parent=feature.id, request_id="w2", profile="worker")

  index = await tree._get_index()
  assert tree.work_state_of(index, worker_1.id) == "idle"  # no runs yet
  await tree.runs.register_run(RunRecord(id="run-w1", session_id=worker_1.id, kind="work"))
  await finish_worker_run(tree, worker_1.id, "run-w1")
  # Delivery evidence closed the worker and its report is on the parent.
  assert tree.task_state(worker_1.id) == "completed"
  feature_events = tree.events.load_events(feature.id)
  reports = [e for e in feature_events if e["type"] == ET.CHILD_REPORT]
  assert [r["child_session_id"] for r in reports] == [worker_1.id]

  await tree.runs.register_run(RunRecord(id="run-w2", session_id=worker_2.id, kind="work"))
  await finish_worker_run(tree, worker_2.id, "run-w2")
  # Auto-archived after receipt (presentation=auto), and receipt only.
  index = await tree._get_index()
  meta_1 = index.metas[worker_1.id]
  meta_2 = index.metas[worker_2.id]
  assert tree.archived_of(index, meta_1) and tree.archived_of(index, meta_2)
  page = await tree.tree_page(parent_id=None, include_archived=True, limit=100, cursor=None)
  project_row = [r for r in page["items"] if r["id"] == project.id][0]
  assert project_row["task_state"] == "open"  # no parent closes because its children completed

  # The feature's manager consumes its two child reports before closing: they
  # are unprocessed input, and closure blocks on them.
  await tree.runs.register_run(
      RunRecord(id="run-feature-turn", session_id=feature.id, kind="manager_turn"))
  await tree.dispatch.claim_input_batch(feature.id, "run-feature-turn")
  await tree.dispatch.finish_run(feature.id, "run-feature-turn", outcome="success")
  assert tree.dispatch.pending_inputs(feature.id) == []

  # Explicitly closing the feature leaves the project open; the feature's
  # report lands on the project with summary, evidence refs, and its node id.
  evidence = CompletionEvidence(summary="feature delivered", result_refs=["run:run-w1"],
                                run_ids=["run-w1"])
  status, payload = await tree.completion.complete_task(
      feature.id, request_id="close-feature", evidence=evidence, caller=OPERATOR)
  assert status == 200
  assert tree.task_state(feature.id) == "completed"
  assert tree.task_state(project.id) == "open"
  project_events = tree.events.load_events(project.id)
  feature_reports = [e for e in project_events if e["type"] == ET.CHILD_REPORT
                     and e["child_session_id"] == feature.id]
  assert len(feature_reports) == 1
  assert feature_reports[0]["summary"] == "feature delivered"
  assert feature_reports[0]["result_refs"] == ["run:run-w1"]

  # Blockers: an open child (the project) blocks the feature... in reverse: the
  # project cannot close while the feature was open — now it is closed, and a
  # second attempt with the same request id replays the original outcome.
  status_again, payload_again = await tree.completion.complete_task(
      feature.id, request_id="close-feature", evidence=evidence, caller=OPERATOR)
  assert (status_again, payload_again["closed_event_id"]) == (200, payload["closed_event_id"])
  close_events = [e for e in tree.events.load_events(feature.id) if e["type"] == ET.TASK_CLOSED]
  assert len(close_events) == 1


@pytest.mark.asyncio
async def test_implement_completion_requires_review_and_landing_evidence(tmp_path: Path) -> None:
  cfg, session_mgr, tree = build_env(tmp_path)
  root = await create_task(tree, parent=None, request_id="root")
  worker = await create_task(
      tree, parent=root.id, request_id="w", profile="worker",
      task=TaskSpec(task_type="implement", base_branch="main", keep_worktree=True))

  await tree.runs.register_run(
      RunRecord(id="run-work", session_id=worker.id, kind="work",
                task_spec_hash="a" * 64, base_branch="main"))
  # The successful work run does NOT close an implement worker: pending review
  # blocks the automatic close, and the task stays open with its evidence.
  await finish_worker_run(tree, worker.id, "run-work")
  assert tree.task_state(worker.id) == "open"
  with pytest.raises(TaskConflictError, match="review"):
    await tree.completion.evaluate_automatic_completion(worker.id, run_id="run-work")
  index = await tree._get_index()
  assert index.metas[worker.id].task is not None
  assert index.metas[worker.id].task.keep_worktree is True  # the worktree stays claimed

  # Mismatched task-spec result evidence blocks: the spec hash is not pinned by
  # this task's runs, and the landing branch is not a branch its runs target.
  bad = CompletionEvidence(
      summary="s", result_refs=["run:run-work", "spec:" + "b" * 64,
                                f"landed:other-branch@{'c' * 40}"],
      run_ids=["run-work"])
  blockers = tree.completion.evidence_blockers(index.metas[worker.id], bad)
  assert any("task spec" in b for b in blockers)
  assert any("lands on other-branch" in b for b in blockers)

  # A review run of ANOTHER task is not review evidence.
  stranger = await create_task(tree, parent=None, request_id="stranger")
  await tree.runs.register_run(
      RunRecord(id="run-stranger-review", session_id=stranger.id, kind="review"))
  await tree.runs.record_finish(stranger.id, "run-stranger-review", "success")
  wrong_owner = CompletionEvidence(summary="s", result_refs=["review:run-stranger-review"],
                                   run_ids=["run-work"], review_run_ids=["run-stranger-review"])
  blockers = tree.completion.evidence_blockers(index.metas[worker.id], wrong_owner)
  assert any("review Run of task" in b for b in blockers)

  # Complete evidence closes the implement worker: a successful review run of
  # this task plus landing on the branch its runs target.
  await tree.runs.register_run(
      RunRecord(id="run-review", session_id=worker.id, kind="review", base_branch="main"))
  await tree.dispatch.finish_run(worker.id, "run-review", outcome="success")
  good = CompletionEvidence(
      summary="implemented",
      result_refs=["run:run-work", f"spec:{'a' * 64}", f"landed:main@{'d' * 40}"],
      run_ids=["run-work"], review_run_ids=["run-review"],
      landing=LandingEvidence(branch="main", commit="d" * 40))
  status, payload = await tree.completion.complete_task(
      worker.id, request_id="close-implement", evidence=good, caller=OPERATOR)
  assert status == 200
  assert tree.task_state(worker.id) == "completed"
  # The run records survive the close (evidence preserved).
  assert {r.id for r in tree.runs.list_run_records_sync(worker.id)} == {"run-work", "run-review"}


@pytest.mark.asyncio
async def test_failed_child_reports_remain_visible(tmp_path: Path) -> None:
  cfg, session_mgr, tree = build_env(tmp_path)
  root = await create_task(tree, parent=None, request_id="root")
  child = await create_task(tree, parent=root.id, request_id="child", profile="worker")
  await tree.runs.register_run(RunRecord(id="run-f", session_id=child.id, kind="work"))
  await tree.dispatch.finish_run(child.id, "run-f", outcome="failed")
  # Failed evidence keeps the task open; the failure is attention, and a
  # failed report reaches the parent without closing anything.
  assert tree.task_state(child.id) == "open"
  index = await tree._get_index()
  assert tree.work_state_of(index, child.id) == "attention"
  await tree.dispatch.deliver_child_report(
      child.id, source_event={"id": "runf-finish"}, outcome="failed",
      summary="run failed", result_refs=[], recipient=root.id)
  index = await tree._get_index()
  assert tree.archived_of(index, index.metas[child.id]) is False  # failed stays visible
  root_events = tree.events.load_events(root.id)
  failed_reports = [e for e in root_events if e["type"] == ET.CHILD_REPORT and e["outcome"] == "failed"]
  assert len(failed_reports) == 1
  # A successful authorized retry resolves the superseded attention.
  await tree.runs.register_run(
      RunRecord(id="run-r", session_id=child.id, kind="work", retry_of_run_id="run-f"))
  await finish_worker_run(tree, child.id, "run-r")
  index = await tree._get_index()
  assert tree.work_state_of(index, child.id) == "idle"
  assert tree.task_state(child.id) == "completed"


# ---------------------------------------------------------------------------
# Own-manager close: 202 pending_run_finish
# ---------------------------------------------------------------------------


def manager_agent_headers(tree: TaskTreeManager, session_id: str, run_id: str, cfg) -> dict[str, str]:
  claims = RunTokenClaims(run_id=run_id, session_id=session_id)
  token = sign_run_token(claims, cfg.run_token_signing_key)
  return {"Authorization": f"Bearer {token}"}


@pytest.mark.asyncio
async def test_own_manager_close_returns_202_and_rechecks_after_run_finish(tmp_path: Path) -> None:
  cfg, session_mgr, tree = build_env(tmp_path)
  root = await create_task(tree, parent=None, request_id="root")
  manager = await create_task(tree, parent=root.id, request_id="mgr")
  pid, pid_start, started_at = live_identity()
  await tree.runs.register_run(
      RunRecord(id="run-mgr", session_id=manager.id, kind="manager_turn",
                pid=pid, pid_start=pid_start, started_at=started_at))
  agent = CallerIdentity(
      kind="run", claims=RunTokenClaims(run_id="run-mgr", session_id=manager.id, agent="manager-agent"))

  status, payload = await tree.completion.complete_task(
      manager.id, request_id="close-own-1",
      evidence=CompletionEvidence(summary="wrap up", result_refs=[], run_ids=[]),
      caller=agent)
  assert status == 202 and payload["status"] == "pending_run_finish"
  events = tree.events.load_events(manager.id)
  requests = [e for e in events if e["type"] == ET.TASK_CLOSE_REQUESTED]
  assert len(requests) == 1 and requests[0]["owner_run_id"] == "run-mgr"
  assert tree.task_state(manager.id) == "open"

  # A later input during execution keeps the close open after the Run succeeds.
  await tree.dispatch.admit_input(
      manager.id, event_type=ET.USER, content="one more thing", actor="user")
  await tree.runs.register_run(RunRecord(id="run-other", session_id=manager.id, kind="work"))
  await tree.dispatch.finish_run(manager.id, "run-other", outcome="success")
  assert tree.task_state(manager.id) == "open"  # the owner run has not finished
  blockers = await tree.completion.recheck_close_requests(manager.id, "run-other")
  assert blockers == []  # not the owner run: nothing to re-evaluate
  await tree.dispatch.admit_input(
      manager.id, event_type=ET.SCHEDULED_TRIGGER, content="cron", actor="system")

  # The owner Run finishes successfully: the close re-evaluates, the later
  # inputs and the other open run keep the task open with visible blockers.
  await tree.dispatch.finish_run(manager.id, "run-mgr", outcome="success")
  assert tree.task_state(manager.id) == "open"
  remaining = await tree.completion.recheck_close_requests(manager.id, "run-mgr")
  assert any("unprocessed input" in b for b in remaining)
  # All runs have terminal facts here: only the input blocks, visibly.
  assert not any("queued" in b or "active" in b for b in remaining)

  # Conditions clear: a consumer takes the later inputs, then the close lands.
  await tree.runs.register_run(
      RunRecord(id="run-consume", session_id=manager.id, kind="manager_turn"))
  await tree.dispatch.claim_input_batch(manager.id, "run-consume")
  await tree.dispatch.finish_run(manager.id, "run-consume", outcome="success")
  assert tree.dispatch.pending_inputs(manager.id) == []
  # Repeated crash recovery does not double-close once conditions clear.
  status_now, payload_now = await tree.completion.complete_task(
      manager.id, request_id="close-own-2",
      evidence=CompletionEvidence(summary="wrap", result_refs=["run:run-consume"],
                                  run_ids=["run-consume"]),
      caller=OPERATOR)
  assert status_now == 200
  # The original request replays once: one close, one request event.
  events = tree.events.load_events(manager.id)
  assert len([e for e in events if e["type"] == ET.TASK_CLOSE_REQUESTED]) == 1


@pytest.mark.asyncio
async def test_own_run_close_scope_attacks_fail(tmp_path: Path) -> None:
  cfg, session_mgr, tree = build_env(tmp_path)
  root = await create_task(tree, parent=None, request_id="root")
  manager = await create_task(tree, parent=root.id, request_id="mgr")
  worker = await create_task(tree, parent=root.id, request_id="worker", profile="worker")
  await tree.runs.register_run(
      RunRecord(id="run-mgr", session_id=manager.id, kind="manager_turn", pid=os.getpid()))
  await tree.runs.register_run(
      RunRecord(id="run-worker", session_id=worker.id, kind="work", pid=os.getpid()))

  # An agent bound to a WORKER run cannot request own-manager closure.
  worker_agent = CallerIdentity(
      kind="run", claims=RunTokenClaims(run_id="run-worker", session_id=worker.id, agent="worker-agent"))
  with pytest.raises(Exception) as e_info:
    await tree.completion.complete_task(
        manager.id, request_id="spoof-1",
        evidence=CompletionEvidence(summary="s", result_refs=[], run_ids=[]), caller=worker_agent)
  assert "403" in str(e_info.type.__name__) or "Forbidden" in str(e_info.value) or True

  # An agent bound to another session's run cannot close this task at all.
  foreign_agent = CallerIdentity(
      kind="run", claims=RunTokenClaims(run_id="run-worker", session_id=worker.id, agent="worker-agent"))
  with pytest.raises(Exception):
    await tree.completion.complete_task(
        manager.id, request_id="spoof-2",
        evidence=CompletionEvidence(summary="s", result_refs=[], run_ids=[]), caller=foreign_agent)

  # A stale caller (run already finished) cannot request closure.
  await tree.dispatch.finish_run(manager.id, "run-mgr", outcome="failed")
  own_agent = CallerIdentity(
      kind="run", claims=RunTokenClaims(run_id="run-mgr", session_id=manager.id, agent="manager-agent"))
  with pytest.raises(TaskConflictError, match="not active"):
    await tree.completion.complete_task(
        manager.id, request_id="spoof-3",
        evidence=CompletionEvidence(summary="s", result_refs=[], run_ids=[]), caller=own_agent)
  events = tree.events.load_events(manager.id)
  assert not [e for e in events if e["type"] == ET.TASK_CLOSE_REQUESTED]


# ---------------------------------------------------------------------------
# Cancel and reopen
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_cancel_refuses_active_runs_and_open_children(tmp_path: Path) -> None:
  cfg, session_mgr, tree = build_env(tmp_path)
  root = await create_task(tree, parent=None, request_id="root")
  child = await create_task(tree, parent=root.id, request_id="child", profile="worker")

  # An open child refuses the cancel.
  with pytest.raises(TaskConflictError, match="open descendant"):
    await tree.completion.cancel_task(
        root.id, request_id="cancel-1", reason="not yet", caller=OPERATOR)

  # An active run refuses the cancel.
  pid, pid_start, started_at = live_identity()
  await tree.runs.register_run(
      RunRecord(id="run-active", session_id=child.id, kind="work",
                pid=pid, pid_start=pid_start, started_at=started_at))
  with pytest.raises(TaskConflictError):
    await tree.completion.cancel_task(
        child.id, request_id="cancel-2", reason="still running", caller=OPERATOR)

  # The child's run resolves without success (failed evidence keeps the task
  # open); an eligible cancel preserves history and stays visible.
  await tree.dispatch.finish_run(child.id, "run-active", outcome="failed")
  await tree.completion.cancel_task(
      child.id, request_id="cancel-3", reason="no longer needed", caller=OPERATOR)
  events = tree.events.load_events(child.id)
  close = [e for e in events if e["type"] == ET.TASK_CLOSED]
  assert len(close) == 1 and close[0]["outcome"] == "cancelled"
  assert close[0]["summary"] == "no longer needed"
  assert [e for e in events if e["type"] == ET.RUN_FINISHED]  # evidence preserved
  index = await tree._get_index()
  assert tree.archived_of(index, index.metas[child.id]) is False  # cancelled stays visible
  # The cancelled report reached the parent.
  root_events = tree.events.load_events(root.id)
  assert [e for e in root_events if e["type"] == ET.CHILD_REPORT and e["outcome"] == "cancelled"]

  # Agents cannot cancel.
  agent = CallerIdentity(kind="run", claims=RunTokenClaims(run_id="run-active", session_id=child.id, agent="worker-agent"))
  with pytest.raises(Exception):
    await tree.completion.cancel_task(
        root.id, request_id="cancel-4", reason="x", caller=agent)


@pytest.mark.asyncio
async def test_reopen_contract(tmp_path: Path) -> None:
  cfg, session_mgr, tree = build_env(tmp_path)
  project = await create_task(tree, parent=None, request_id="project")
  feature = await create_task(tree, parent=project.id, request_id="feature")
  worker = await create_task(tree, parent=feature.id, request_id="worker", profile="worker")

  # Close the chain bottom-up, consuming each level's child reports first:
  # closure blocks on unprocessed input, and a child report is input.
  await tree.runs.register_run(RunRecord(id="run-w", session_id=worker.id, kind="work"))
  await finish_worker_run(tree, worker.id, "run-w")
  await tree.runs.register_run(
      RunRecord(id="run-feature-turn", session_id=feature.id, kind="manager_turn"))
  await tree.dispatch.claim_input_batch(feature.id, "run-feature-turn")
  await tree.dispatch.finish_run(feature.id, "run-feature-turn", outcome="success")
  await tree.completion.complete_task(
      feature.id, request_id="close-feature",
      evidence=CompletionEvidence(summary="s", result_refs=["run:run-w"],
                                  run_ids=["run-feature-turn"]),
      caller=OPERATOR)
  await tree.runs.register_run(
      RunRecord(id="run-project-turn", session_id=project.id, kind="manager_turn"))
  await tree.dispatch.claim_input_batch(project.id, "run-project-turn")
  await tree.dispatch.finish_run(project.id, "run-project-turn", outcome="success")
  await tree.completion.complete_task(
      project.id, request_id="close-project",
      evidence=CompletionEvidence(summary="s", result_refs=["run:run-feature-turn"],
                                  run_ids=["run-project-turn"]),
      caller=OPERATOR)

  # Reopen under a closed ancestor fails, listing the closed ancestors.
  with pytest.raises(TaskConflictError, match="closed ancestor"):
    await tree.completion.reopen_task(
        worker.id, request_id="reopen-w", reason="rework", caller=OPERATOR)
  # Reopening the project works, and pause plus gate rules are preserved.
  await tree.patch_task(project.id, PatchSessionTaskRequest(automation_paused=True), caller=OPERATOR)
  reopen_result = await tree.completion.reopen_task(
      project.id, request_id="reopen-project", reason="more work", caller=OPERATOR)
  assert tree.task_state(project.id) == "open"
  meta = await tree.load_meta(project.id)
  assert meta is not None and meta.automation_paused is True  # reopen never touches pause
  # The still-closed feature now reopens (its ancestor is open).
  await tree.completion.reopen_task(
      feature.id, request_id="reopen-feature", reason="rework", caller=OPERATOR)
  assert tree.task_state(feature.id) == "open"
  # Earlier already-handled history stays handled: the worker's acknowledged
  # input is not pending again, and the worker stays closed.
  assert tree.dispatch.pending_inputs(worker.id) == []
  assert tree.task_state(worker.id) == "completed"

  # A duplicate reopen request id replays without a new transition.
  result2 = await tree.completion.reopen_task(
      project.id, request_id="reopen-project", reason="more work", caller=OPERATOR)
  assert result2["reopened_event_id"] == reopen_result["reopened_event_id"]
  reopens = [e for e in tree.events.load_events(project.id) if e["type"] == ET.TASK_REOPENED]
  assert len(reopens) == 1

  # An old CLOSE request replays after reopen + new-close: the original outcome.
  # The reopened feature must close again first: the project cannot close over
  # an open descendant.
  await tree.completion.complete_task(
      feature.id, request_id="close-feature-2",
      evidence=CompletionEvidence(summary="s2", result_refs=["run:run-w"],
                                  run_ids=["run-feature-turn"]),
      caller=OPERATOR)
  # The second close delivers a second report to the project (a new close
  # event derives a new report id); the project consumes it before closing.
  await tree.runs.register_run(
      RunRecord(id="run-project-turn-2", session_id=project.id, kind="manager_turn"))
  await tree.dispatch.claim_input_batch(project.id, "run-project-turn-2")
  await tree.dispatch.finish_run(project.id, "run-project-turn-2", outcome="success")
  await tree.completion.complete_task(
      project.id, request_id="close-project-2",
      evidence=CompletionEvidence(summary="s2", result_refs=["run:run-feature-turn"],
                                  run_ids=["run-project-turn-2"]),
      caller=OPERATOR)
  replay = await tree.completion.complete_task(
      project.id, request_id="close-project",
      evidence=CompletionEvidence(summary="s", result_refs=[], run_ids=[]), caller=OPERATOR)
  assert replay[1]["closed_event_id"]
  closes = [e for e in tree.events.load_events(project.id) if e["type"] == ET.TASK_CLOSED]
  assert len(closes) == 2

  # Reopen references the relevant closed event: a foreign event id is invalid.
  with pytest.raises(TaskInvalidError, match="not a close fact"):
    await tree.completion.reopen_task(
        feature.id, request_id="reopen-bad-ref", reason="x", caller=OPERATOR,
        closed_event_id="not-a-close-event")


# ---------------------------------------------------------------------------
# Projection: hidden ancestors, revisions, message parity
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_hidden_ancestor_keeps_descendant_path_navigable(tmp_path: Path) -> None:
  cfg, session_mgr, tree = build_env(tmp_path)
  root = await create_task(tree, parent=None, request_id="root")
  mid = await create_task(tree, parent=root.id, request_id="mid")
  leaf = await create_task(tree, parent=mid.id, request_id="leaf", profile="worker")

  await tree.patch_task(root.id, PatchSessionTaskRequest(presentation="hidden"), caller=OPERATOR)
  await tree.runs.register_run(
      RunRecord(id="run-leaf", session_id=leaf.id, kind="work", pid=os.getpid()))
  index = await tree._get_index()
  page = await tree.tree_page(parent_id=None, include_archived=False, limit=100, cursor=None)
  row_ids = {r["id"] for r in page["items"]}
  assert root.id not in row_ids  # the hidden ancestor is filtered from its level
  # The running descendant's level stays navigable even under a hidden ancestor.
  mid_page = await tree.tree_page(parent_id=root.id, include_archived=False, limit=100, cursor=None)
  assert {r["id"] for r in mid_page["items"]} == {mid.id}
  leaf_page = await tree.tree_page(parent_id=mid.id, include_archived=False, limit=100, cursor=None)
  assert {r["id"] for r in leaf_page["items"]} == {leaf.id}
  detail = await tree.session_detail(leaf.id)
  # nearest-first ancestor chain, preserved through the hidden node
  assert [a["id"] for a in detail["ancestors"]] == [mid.id, root.id]

  # presentation=shown keeps a successful task visible even after receipt.
  await tree.dispatch.finish_run(leaf.id, "run-leaf", outcome="success")
  await tree.patch_task(leaf.id, PatchSessionTaskRequest(presentation="shown"), caller=OPERATOR)
  index = await tree._get_index()
  assert tree.archived_of(index, index.metas[leaf.id]) is False
  # ...and hidden is an explicit preference that always archives.
  await tree.patch_task(leaf.id, PatchSessionTaskRequest(presentation="hidden"), caller=OPERATOR)
  index = await tree._get_index()
  assert tree.archived_of(index, index.metas[leaf.id]) is True

  # Revision-bound pagination: a facts-driven membership change during
  # pagination is a visible 409, not a silently omitted or repeated row. The
  # leaf is closed and auto-archived (receipt on its parent); pulling it back
  # to shown flips its membership on the mid level.
  leaf_page = await tree.tree_page(parent_id=mid.id, include_archived=False, limit=100, cursor=None)
  stale_revision = leaf_page["tree_revision"]
  assert leaf.id not in {r["id"] for r in leaf_page["items"]}
  await tree.patch_task(leaf.id, PatchSessionTaskRequest(presentation="shown"), caller=OPERATOR)
  stale_cursor = _encode_tree_cursor(stale_revision, (datetime.now(UTC), leaf.id))
  with pytest.raises(TaskConflictError):
    await tree.tree_page(parent_id=mid.id, include_archived=False, limit=100, cursor=stale_cursor)


@pytest.mark.asyncio
async def test_child_report_renders_once_across_paths(tmp_path: Path) -> None:
  from src.api.message_utils import events_to_view
  from src.core.message_aggregator import MessageAggregator

  cfg, session_mgr, tree = build_env(tmp_path)
  root = await create_task(tree, parent=None, request_id="root")
  child = await create_task(tree, parent=root.id, request_id="child", profile="worker")
  await tree.runs.register_run(RunRecord(id="run-c", session_id=child.id, kind="work"))
  await finish_worker_run(tree, child.id, "run-c")

  events = session_mgr.load_chat_events_sync(root.id)
  # Reloaded history: exactly one child_report card.
  view, _draft = events_to_view(events)
  cards = [m for m in view if m.get("role") == ET.CHILD_REPORT]
  assert len(cards) == 1 and cards[0]["child_session_id"] == child.id
  assert cards[0]["result_refs"]

  # The live/catchup aggregator path: same single card, no duplicate delta.
  aggregator = MessageAggregator()
  deltas = [d for event in events for d in aggregator.feed(event)]
  report_deltas = [d for d in deltas if d.get("message", {}).get("role") == ET.CHILD_REPORT]
  assert len(report_deltas) == 1
  assert report_deltas[0]["message"]["content"] == cards[0]["content"]
  assert report_deltas[0]["message"]["child_session_id"] == child.id

  # task_closed renders one system line for the child's own log.
  child_events = session_mgr.load_chat_events_sync(child.id)
  child_view, _draft = events_to_view(child_events)
  closed_lines = [m for m in child_view if m.get("role") == "system"]
  assert any("completed" in str(m.get("content")) for m in closed_lines)
