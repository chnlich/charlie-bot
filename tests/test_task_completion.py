"""Completion-stage tests: evidence, close/cancel/reopen, projection, routes."""

from __future__ import annotations

import asyncio
import subprocess
from datetime import UTC, datetime
from pathlib import Path

import pytest
from conftest import (
    MASTER_TRIGGER_TRIGGER_MASTER_PATCH_TARGET,
    OPERATOR,
    OPUS_BACKEND_ID,
    build_env,
    create_task,
)

from src.core import event_types as ET
from src.core.models import CreateSessionRequest, RunRecord, TaskSpec
from src.core.run_token import CallerIdentity, RunTokenClaims
from src.core.task_completion import CompletionEvidence, LandingEvidence
from src.core.task_sessions import (
    TaskConflictError,
    TaskForbiddenError,
    TaskTreeManager,
)


def live_identity() -> tuple[int, str, datetime]:
  """An owned, isolated live process identity (never a zombie or a fake)."""
  proc = subprocess.Popen(["/bin/sleep", "30"])
  from src.core.runs import read_pid_stat
  pair = read_pid_stat(proc.pid)
  assert pair is not None
  return proc.pid, pair[0], datetime.now(UTC)


async def finish_worker_run(tree: TaskTreeManager, session_id: str, run_id: str) -> None:
  """Land one successful worker work run: the automatic completion path."""
  await tree.dispatch.finish_run(session_id, run_id, outcome="success")


async def legacy_parent_and_task(tree: TaskTreeManager, session_mgr, *, request_id: str):
  """A legacy (profile None) parent session with one fresh child task under it.

  This is the shape the 9/25 echo incident ran on: a master session's own turn
  closed a delegated task and the scheduled wake replayed its own words.
  """
  legacy = await session_mgr.create_session(CreateSessionRequest(name="Legacy"), backend=OPUS_BACKEND_ID)
  child = await create_task(tree, parent=legacy.id, request_id=request_id)
  return legacy, child


def wake_probe():
  """A trigger_master stand-in that records (parent_id, text) and signals fired.

  The legacy wake is scheduled fire-and-forget, so the probe signals through an
  event: awaiting it proves the wake ran; a timed-out wait proves it never did.
  """
  calls: list[tuple[str, str]] = []
  fired = asyncio.Event()

  async def trigger(parent_id: str, text: str, *_rest: object) -> None:
    calls.append((parent_id, text))
    fired.set()

  return trigger, calls, fired


async def assert_wake_stays_quiet(fired: asyncio.Event) -> None:
  """Give any wrongly scheduled wake a bounded loop wait, then require silence."""
  with pytest.raises(asyncio.TimeoutError):
    await asyncio.wait_for(fired.wait(), timeout=0.5)


def persisted_close_and_report(tree: TaskTreeManager, parent_id: str, child_id: str,
                               outcome: str) -> tuple[list[dict], list[dict]]:
  child_events = tree.events.load_events(child_id)
  closes = [e for e in child_events if e["type"] == ET.TASK_CLOSED and e["outcome"] == outcome]
  parent_events = tree.events.load_events(parent_id)
  reports = [e for e in parent_events if e["type"] == ET.CHILD_REPORT and e["child_session_id"] == child_id]
  return closes, reports


# ---------------------------------------------------------------------------
# Three-level delivery: workers close, explicit feature close, project open
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_three_level_delivery_closes_workers_and_keeps_project_open(tmp_path: Path) -> None:
  _cfg, _session_mgr, tree = build_env(tmp_path)
  project = await create_task(tree, parent=None, request_id="project")
  feature = await create_task(tree, parent=project.id, request_id="feature")
  worker_1 = await create_task(tree, parent=feature.id, request_id="w1", profile="worker")
  worker_2 = await create_task(tree, parent=feature.id, request_id="w2", profile="worker")

  assert tree.work_state_of(worker_1.id) == "idle"  # no runs yet
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
  await tree.runs.register_run(RunRecord(id="run-feature-turn", session_id=feature.id, kind="manager_turn"))
  await tree.dispatch.claim_input_batch(feature.id, "run-feature-turn")
  await tree.dispatch.finish_run(feature.id, "run-feature-turn", outcome="success")
  assert tree.dispatch.pending_inputs(feature.id) == []

  # Explicitly closing the feature leaves the project open; the feature's
  # report lands on the project with summary, evidence refs, and its node id.
  evidence = CompletionEvidence(summary="feature delivered", result_refs=["run:run-w1"], run_ids=["run-w1"])
  status, payload = await tree.completion.complete_task(
      feature.id, request_id="close-feature", evidence=evidence, caller=OPERATOR)
  assert status == 200
  assert tree.task_state(feature.id) == "completed"
  assert tree.task_state(project.id) == "open"
  project_events = tree.events.load_events(project.id)
  feature_reports = [e for e in project_events if e["type"] == ET.CHILD_REPORT and e["child_session_id"] == feature.id]
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
  _cfg, _session_mgr, tree = build_env(tmp_path)
  root = await create_task(tree, parent=None, request_id="root")
  worker = await create_task(
      tree,
      parent=root.id,
      request_id="w",
      profile="worker",
      task=TaskSpec(task_type="implement", base_branch="main", keep_worktree=True))

  await tree.runs.register_run(
      RunRecord(id="run-work", session_id=worker.id, kind="work", task_spec_hash="a" * 64, base_branch="main"))
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
      summary="s",
      result_refs=["run:run-work", "spec:" + "b" * 64, f"landed:other-branch@{'c' * 40}"],
      run_ids=["run-work"])
  blockers = tree.completion.evidence_blockers(index.metas[worker.id], bad)
  assert any("task spec" in b for b in blockers)
  assert any("lands on other-branch" in b for b in blockers)

  # A review run that chains to a different work Run is not this delivery's
  # evidence (the work/spec/review pin): it names work this claim does not.
  await tree.runs.register_run(
      RunRecord(id="run-misreview", session_id=worker.id, kind="review", review_of_run_id="run-earlier-attempt"))
  await tree.runs.record_finish(worker.id, "run-misreview", "success")
  mischained = CompletionEvidence(
      summary="s", result_refs=["review:run-misreview"], run_ids=["run-work"], review_run_ids=["run-misreview"])
  blockers = tree.completion.evidence_blockers(index.metas[worker.id], mischained)
  assert any("reviews work run run-earlier-attempt" in b for b in blockers)

  # A review run of ANOTHER task is not review evidence.
  stranger = await create_task(tree, parent=None, request_id="stranger")
  await tree.runs.register_run(RunRecord(id="run-stranger-review", session_id=stranger.id, kind="review"))
  await tree.runs.record_finish(stranger.id, "run-stranger-review", "success")
  wrong_owner = CompletionEvidence(
      summary="s",
      result_refs=["review:run-stranger-review"],
      run_ids=["run-work"],
      review_run_ids=["run-stranger-review"])
  blockers = tree.completion.evidence_blockers(index.metas[worker.id], wrong_owner)
  assert any("review Run of task" in b for b in blockers)

  # Complete evidence closes the implement worker: a successful review run of
  # this task (chained to the named work Run) plus a commit that REALLY landed
  # on the target branch of the task's repository — the git layer verifies
  # existence and ancestry, so a forged or unlanded hash cannot close.
  repo = tmp_path / "task-repo"
  subprocess.run(["git", "init", "-q", "-b", "main", str(repo)], check=True)
  subprocess.run(["git", "-C", str(repo), "config", "user.email", "t@example.com"], check=True)
  subprocess.run(["git", "-C", str(repo), "config", "user.name", "t"], check=True)
  (repo / "seed.txt").write_text("seed\n")
  subprocess.run(["git", "-C", str(repo), "add", "."], check=True)
  subprocess.run(["git", "-C", str(repo), "commit", "-q", "-m", "seed"], check=True)
  tree_meta = await tree.load_meta(worker.id)
  assert tree_meta is not None and tree_meta.task is not None
  tree_meta.task.repo_path = str(repo)
  tree_meta.task.base_branch = "main"
  await tree._save_meta(tree_meta)
  landed_commit = subprocess.run(
      ["git", "-C", str(repo), "rev-parse", "HEAD"], check=True, capture_output=True, text=True).stdout.strip()

  await tree.runs.register_run(
      RunRecord(id="run-review", session_id=worker.id, kind="review", base_branch="main", review_of_run_id="run-work"))
  await tree.dispatch.finish_run(worker.id, "run-review", outcome="success")
  good = CompletionEvidence(
      summary="implemented",
      result_refs=["run:run-work", f"spec:{'a' * 64}", f"landed:main@{landed_commit}"],
      run_ids=["run-work"],
      review_run_ids=["run-review"],
      landing=LandingEvidence(branch="main", commit=landed_commit, repo_path=str(repo)))

  # The forged variant of the same shape — a hash that exists nowhere — is an
  # explicit verified blocker on the manual path.
  forged = CompletionEvidence(
      summary="forged",
      result_refs=["run:run-work", f"landed:main@{'d' * 40}"],
      run_ids=["run-work"],
      review_run_ids=["run-review"],
      landing=LandingEvidence(branch="main", commit="d" * 40, repo_path=str(repo)))
  with pytest.raises(TaskConflictError, match="landing evidence unverified"):
    await tree.completion.complete_task(worker.id, request_id="close-forged", evidence=forged, caller=OPERATOR)
  assert tree.task_state(worker.id) == "open"
  status, _payload = await tree.completion.complete_task(
      worker.id, request_id="close-implement", evidence=good, caller=OPERATOR)
  assert status == 200
  assert tree.task_state(worker.id) == "completed"
  # The run records survive the close (evidence preserved).
  assert {r.id for r in tree.runs.list_run_records_sync(worker.id)} == {"run-work", "run-review", "run-misreview"}


# ---------------------------------------------------------------------------
# Own-manager close: 202 pending_run_finish
# ---------------------------------------------------------------------------


async def live_manager_caller(tree: TaskTreeManager, session_id: str, run_id: str) -> CallerIdentity:
  """Register one live manager_turn Run and return the agent caller its token binds."""
  pid, pid_start, started_at = live_identity()
  await tree.runs.register_run(
      RunRecord(
          id=run_id, session_id=session_id, kind="manager_turn", pid=pid, pid_start=pid_start, started_at=started_at))
  return CallerIdentity(kind="agent", claims=RunTokenClaims(run_id=run_id, session_id=session_id, agent="manager"))


def close_requests_of(tree: TaskTreeManager, session_id: str) -> list[dict]:
  return [e for e in tree.events.load_events(session_id) if e["type"] == ET.TASK_CLOSE_REQUESTED]


@pytest.mark.asyncio
async def test_own_run_request_naming_a_foreign_run_is_refused_and_not_saved(tmp_path: Path) -> None:
  """A run id that is not a Run of the task or its children (the 9/27 shape: a
  backend's own run id) is refused at request time instead of a 202 the
  re-evaluation could only block."""
  _cfg, _session_mgr, tree = build_env(tmp_path)
  root = await create_task(tree, parent=None, request_id="root")
  manager = await create_task(tree, parent=root.id, request_id="mgr")
  agent = await live_manager_caller(tree, manager.id, "run-mgr")

  evidence = CompletionEvidence(summary="done", result_refs=["report:done"], run_ids=["run-mgr", "rollout-1"])
  with pytest.raises(TaskConflictError) as refused:
    await tree.completion.complete_task(manager.id, request_id="close-1", evidence=evidence, caller=agent)
  assert any("rollout-1 is not a Run of task" in b for b in refused.value.blockers)
  # The caller's own still-active Run is no blocker: it is judged as the success it must become.
  assert not any("run-mgr" in b for b in refused.value.blockers)
  assert close_requests_of(tree, manager.id) == []
  assert tree.task_state(manager.id) == "open"


@pytest.mark.asyncio
async def test_own_run_request_with_an_open_child_is_refused_and_not_saved(tmp_path: Path) -> None:
  _cfg, _session_mgr, tree = build_env(tmp_path)
  root = await create_task(tree, parent=None, request_id="root")
  manager = await create_task(tree, parent=root.id, request_id="mgr")
  child = await create_task(tree, parent=manager.id, request_id="child", profile="worker")
  agent = await live_manager_caller(tree, manager.id, "run-mgr")

  evidence = CompletionEvidence(summary="done", result_refs=["report:done"], run_ids=["run-mgr"])
  with pytest.raises(TaskConflictError) as refused:
    await tree.completion.complete_task(manager.id, request_id="close-1", evidence=evidence, caller=agent)
  assert f"has open descendant task {child.id}" in refused.value.blockers
  assert close_requests_of(tree, manager.id) == []
  assert tree.task_state(manager.id) == "open"


@pytest.mark.asyncio
async def test_saved_request_blocked_after_its_run_wakes_the_requester_once(tmp_path: Path) -> None:
  """A valid request is saved; input arriving during the requesting Run blocks
  the close after that Run succeeds. The requester gets one wake input naming
  the request and its blockers — never a user event — and a recovery replay of
  the same request adds no second notice."""
  from src.core.task_recovery import reconcile_task_tree

  cfg, _session_mgr, tree = build_env(tmp_path)
  root = await create_task(tree, parent=None, request_id="root")
  manager = await create_task(tree, parent=root.id, request_id="mgr")
  agent = await live_manager_caller(tree, manager.id, "run-mgr")

  evidence = CompletionEvidence(summary="done", result_refs=["report:done"], run_ids=["run-mgr"])
  status, payload = await tree.completion.complete_task(
      manager.id, request_id="close-1", evidence=evidence, caller=agent)
  assert (status, payload["status"]) == (202, "pending_run_finish")
  assert len(close_requests_of(tree, manager.id)) == 1

  late = await tree.dispatch.admit_input(manager.id, event_type=ET.USER, content="one more thing", actor="user")
  await tree.dispatch.finish_run(manager.id, "run-mgr", outcome="success")
  assert tree.task_state(manager.id) == "open"

  new_inputs = [e for e in tree.dispatch.pending_inputs(manager.id) if e["id"] != late["id"]]
  assert len(new_inputs) == 1
  notice = new_inputs[0]
  assert notice["type"] not in (ET.USER, ET.AGENT_MESSAGE)
  assert "close-1" in notice["content"]
  assert f"has unprocessed input: {late['id']}" in notice["content"]

  await reconcile_task_tree(cfg, tree)
  assert tree.task_state(manager.id) == "open"
  assert [e["id"] for e in tree.events.load_events(manager.id) if e["type"] == notice["type"]] == [notice["id"]]


# ---------------------------------------------------------------------------
# Cancel and reopen
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_agent_cancels_only_its_own_direct_child(tmp_path: Path) -> None:
  """An agent cancels a direct child of its own task (the close fact records
  the agent as the actor); every other target — grandchild, sibling, parent,
  self, unrelated root — is refused, and an active run still blocks."""
  _cfg, _session_mgr, tree = build_env(tmp_path)
  root = await create_task(tree, parent=None, request_id="root")
  manager = await create_task(tree, parent=root.id, request_id="mgr")
  sibling = await create_task(tree, parent=root.id, request_id="sib")
  worker = await create_task(tree, parent=manager.id, request_id="worker", profile="worker")
  # The grandchild shape: a logical-manager child of the agent's own task with
  # a child of its own — two levels below the agent's task, never a direct one.
  sub = await create_task(tree, parent=manager.id, request_id="sub")
  grandchild = await create_task(tree, parent=sub.id, request_id="grand")
  unrelated = await create_task(tree, parent=None, request_id="unrelated")
  agent = CallerIdentity(
      kind="agent", claims=RunTokenClaims(run_id="run-mgr", session_id=manager.id, agent="manager-agent"))

  # Out of scope: grandchild, sibling, parent, self, and an unrelated root.
  for target, request_id in ((grandchild, "cancel-g"), (sibling, "cancel-s"), (root, "cancel-p"),
                             (manager, "cancel-self"), (unrelated, "cancel-u")):
    with pytest.raises(TaskForbiddenError, match="direct child"):
      await tree.completion.cancel_task(target.id, request_id=request_id, reason="x", caller=agent)

  # Its own direct child cancels; the close fact's actor is the agent, and the
  # cancelled report reaches the caller's own task like any child report.
  await tree.completion.cancel_task(worker.id, request_id="cancel-w", reason="obsolete delegation", caller=agent)
  close = [e for e in tree.events.load_events(worker.id) if e["type"] == ET.TASK_CLOSED]
  assert len(close) == 1 and close[0]["outcome"] == "cancelled" and close[0]["actor"] == "agent"
  reports = [
      e for e in tree.events.load_events(manager.id)
      if e["type"] == ET.CHILD_REPORT and e["child_session_id"] == worker.id
  ]
  assert len(reports) == 1 and reports[0]["outcome"] == "cancelled"

  # An active run on the direct child still refuses the agent (same 409 shape
  # as the operator path: agents never stop a child's run).
  worker2 = await create_task(tree, parent=manager.id, request_id="worker2", profile="worker")
  pid, pid_start, started_at = live_identity()
  await tree.runs.register_run(
      RunRecord(id="run-w2", session_id=worker2.id, kind="work", pid=pid, pid_start=pid_start, started_at=started_at))
  with pytest.raises(TaskConflictError):
    await tree.completion.cancel_task(worker2.id, request_id="cancel-w2", reason="x", caller=agent)


@pytest.mark.asyncio
async def test_cancel_waits_for_a_queued_run_only_until_its_stop_request(tmp_path: Path) -> None:
  """A queued (pid-less) run blocks the cancel like any unresolved execution;
  a durable stop request settles it and the cancel succeeds."""
  _cfg, _session_mgr, tree = build_env(tmp_path)
  root = await create_task(tree, parent=None, request_id="root")
  child = await create_task(tree, parent=root.id, request_id="child", profile="worker")
  await tree.runs.register_run(RunRecord(id="run-queued", session_id=child.id, kind="work"))

  with pytest.raises(TaskConflictError, match="queued"):
    await tree.completion.cancel_task(child.id, request_id="cancel-1", reason="not yet", caller=OPERATOR)

  stop = await tree.runs.request_stop(child.id, "run-queued", "stop-1")
  assert stop.stop_requested is True and stop.outcome is None
  await tree.completion.cancel_task(child.id, request_id="cancel-2", reason="stopped work is settled", caller=OPERATOR)
  close = [e for e in tree.events.load_events(child.id) if e["type"] == ET.TASK_CLOSED]
  assert len(close) == 1 and close[0]["outcome"] == "cancelled"
  # The queued run keeps its no-terminal-fact shape: settled by request, not
  # by an outcome.
  runs = tree.runs.list_run_records_sync(child.id)
  assert len(runs) == 1 and runs[0].pid is None
  assert tree.runs.terminal_outcome(tree.runs.load_events_sync(child.id), "run-queued") is None


# ---------------------------------------------------------------------------
# Projection: hidden ancestors, revisions, message parity
# ---------------------------------------------------------------------------

# ---------------------------------------------------------------------------
# Who closed the gate: the parent's own turn skips the wake, others wake it
# ---------------------------------------------------------------------------


@pytest.mark.integration
@pytest.mark.asyncio
async def test_cancel_by_the_parent_session_skips_the_parent_wake(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The parent's own turn already holds the outcome in its HTTP response; the
  close facts and the report still land, but no echo turn is scheduled."""
  _cfg, session_mgr, tree = build_env(tmp_path)
  legacy, child = await legacy_parent_and_task(tree, session_mgr, request_id="child")
  trigger, calls, fired = wake_probe()
  monkeypatch.setattr(MASTER_TRIGGER_TRIGGER_MASTER_PATCH_TARGET, trigger)

  payload = await tree.completion.cancel_task(
      child.id,
      request_id="cancel-1",
      reason="no longer needed",
      caller=CallerIdentity(kind="operator", session_id=legacy.id))
  await assert_wake_stays_quiet(fired)

  assert payload["closed_event_id"]
  closes, reports = persisted_close_and_report(tree, legacy.id, child.id, "cancelled")
  assert len(closes) == 1 and closes[0]["summary"] == "no longer needed"
  assert len(reports) == 1 and reports[0]["outcome"] == "cancelled"
  assert calls == []
