"""Completion-stage tests: evidence, archive/cancel/restore, projection, routes."""

from __future__ import annotations

import asyncio
import datetime
import pathlib
import subprocess

import conftest
import pytest

from src.infra import event_types as ET
from src.infra import models
from src.runtime import control_events, run_token, runs, task_completion, task_errors, task_sessions


def live_identity() -> tuple[int, str, datetime.datetime]:
  """An owned, isolated live process identity (never a zombie or a fake)."""
  proc = subprocess.Popen(["/bin/sleep", "30"])
  from src.runtime import runs
  pair = runs.read_pid_stat(proc.pid)
  assert pair is not None
  return proc.pid, pair[0], datetime.datetime.now(datetime.UTC)


async def register_isolated_sleep_run(
    tree: task_sessions.TaskTreeManager,
    session_id: str,
    run_id: str,
    *,
    sequence_ref: models.SequenceRef | None = None,
) -> subprocess.Popen:
  """Register a real run process in its own process group."""
  proc = subprocess.Popen(["/bin/sleep", "60"], start_new_session=True)
  pair = runs.read_pid_stat(proc.pid)
  assert pair is not None
  await tree.runs.register_run_locked(
      models.RunRecord(
          id=run_id,
          session_id=session_id,
          kind="work",
          pid=proc.pid,
          pid_start=pair[0],
          started_at=datetime.datetime.now(datetime.UTC),
          sequence_ref=sequence_ref,
      ))
  return proc


async def finish_worker_run(tree: task_sessions.TaskTreeManager, session_id: str, run_id: str) -> None:
  """Land one successful worker work run: the automatic completion path."""
  await tree.dispatch.finish_run(session_id, run_id, outcome="success")


def persisted_close_and_report(tree: task_sessions.TaskTreeManager, parent_id: str, child_id: str,
                               outcome: str) -> tuple[list[dict], list[dict]]:
  child_events = tree.events.load_events(child_id)
  closes = [e for e in child_events if e["type"] == ET.TASK_CLOSED and e["outcome"] == outcome]
  parent_events = tree.events.load_events(parent_id)
  reports = [e for e in parent_events if e["type"] == ET.CHILD_REPORT and e["child_session_id"] == child_id]
  return closes, reports


# ---------------------------------------------------------------------------
# Three-level delivery: workers close, feature archive, project open
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_three_level_delivery_closes_workers_and_keeps_project_open(tmp_path: pathlib.Path) -> None:
  _cfg, _session_blocks, tree = conftest.build_env(tmp_path)
  project = await conftest.create_task(tree, parent=None, request_id="project")
  feature = await conftest.create_task(tree, parent=project.id, request_id="feature")
  worker_1 = await conftest.create_task(tree, parent=feature.id, request_id="w1", profile="worker")
  worker_2 = await conftest.create_task(tree, parent=feature.id, request_id="w2", profile="worker")

  assert tree.activity_of(worker_1.id).work_state == "idle"  # no runs yet
  await tree.runs.register_run_locked(models.RunRecord(id="run-w1", session_id=worker_1.id, kind="work"))
  await finish_worker_run(tree, worker_1.id, "run-w1")
  # Delivery evidence closed the worker and its report is on the parent.
  assert tree.task_state(worker_1.id) == "completed"
  feature_events = tree.events.load_events(feature.id)
  reports = [e for e in feature_events if e["type"] == ET.CHILD_REPORT]
  assert [r["child_session_id"] for r in reports] == [worker_1.id]

  await tree.runs.register_run_locked(models.RunRecord(id="run-w2", session_id=worker_2.id, kind="work"))
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
  await tree.runs.register_run_locked(
      models.RunRecord(id="run-feature-turn", session_id=feature.id, kind="manager_turn"))
  async with tree.control_lock:
    await tree.dispatch.claim_input_batch_locked(feature.id, "run-feature-turn")
  await tree.dispatch.finish_run(feature.id, "run-feature-turn", outcome="success")
  assert tree.dispatch.pending_inputs(feature.id) == []

  # Archiving the feature leaves the project open; the feature's
  # report lands on the project with summary, evidence refs, and its node id.
  evidence = task_completion.CompletionEvidence(
      summary="feature delivered", result_refs=["run:run-w1"], run_ids=["run-w1"])
  status, payload = await tree.completion.archive_task(
      feature.id, request_id="close-feature", evidence=evidence, caller=conftest.OPERATOR)
  assert status == 200
  assert payload == {"archived": [feature.id]}
  assert tree.task_state(feature.id) == "archived"
  assert tree.task_state(project.id) == "open"
  project_events = tree.events.load_events(project.id)
  feature_reports = [e for e in project_events if e["type"] == ET.CHILD_REPORT and e["child_session_id"] == feature.id]
  assert len(feature_reports) == 1
  assert feature_reports[0]["outcome"] == "archived"
  assert feature_reports[0]["summary"] == "feature delivered"
  assert feature_reports[0]["result_refs"] == ["run:run-w1"]

  # Blockers: an open child (the project) blocks the feature... in reverse: the
  # project cannot close while the feature was open — now it is closed, and a
  # A repeated archive of the closed target writes no new facts.
  status_again, payload_again = await tree.completion.archive_task(
      feature.id, request_id="close-feature", evidence=evidence, caller=conftest.OPERATOR)
  close_events = [e for e in tree.events.load_events(feature.id) if e["type"] == ET.TASK_CLOSED]
  assert (status_again, payload_again["closed_event_id"]) == (200, close_events[0]["id"])
  assert len(close_events) == 1


@pytest.mark.asyncio
async def test_implement_completion_requires_review_and_landing_evidence(tmp_path: pathlib.Path) -> None:
  _cfg, _session_blocks, tree = conftest.build_env(tmp_path)
  root = await conftest.create_task(tree, parent=None, request_id="root")
  worker = await conftest.create_task(
      tree,
      parent=root.id,
      request_id="w",
      profile="worker",
      task=models.TaskSpec(task_type="implement", base_branch="main", keep_worktree=True))

  await tree.runs.register_run_locked(
      models.RunRecord(id="run-work", session_id=worker.id, kind="work", task_spec_hash="a" * 64, base_branch="main"))
  # The successful work run does NOT close an implement worker: pending review
  # blocks the automatic close, and the task stays open with its evidence.
  await finish_worker_run(tree, worker.id, "run-work")
  assert tree.task_state(worker.id) == "open"
  with pytest.raises(task_errors.TaskConflictError, match="review"):
    await tree.completion.evaluate_automatic_completion(worker.id, run_id="run-work")
  index = await tree._get_index()
  assert index.metas[worker.id].task is not None
  assert index.metas[worker.id].task.keep_worktree is True  # the worktree stays claimed

  # Mismatched task-spec result evidence blocks: the spec hash is not pinned by
  # this task's runs, and the landing branch is not a branch its runs target.
  bad = task_completion.CompletionEvidence(
      summary="s",
      result_refs=["run:run-work", "spec:" + "b" * 64, f"landed:other-branch@{'c' * 40}"],
      run_ids=["run-work"])
  blockers = tree.completion.evidence_blockers(index, index.metas[worker.id], bad)
  assert any("task spec" in b for b in blockers)
  assert any("lands on other-branch" in b for b in blockers)

  # A review run that chains to a different work Run is not this delivery's
  # evidence (the work/spec/review pin): it names work this claim does not.
  await tree.runs.register_run_locked(
      models.RunRecord(id="run-misreview", session_id=worker.id, kind="review", review_of_run_id="run-earlier-attempt"))
  await tree.runs.record_finish(worker.id, "run-misreview", "success")
  mischained = task_completion.CompletionEvidence(
      summary="s", result_refs=["review:run-misreview"], run_ids=["run-work"], review_run_ids=["run-misreview"])
  blockers = tree.completion.evidence_blockers(index, index.metas[worker.id], mischained)
  assert any("reviews work run run-earlier-attempt" in b for b in blockers)

  # A review run of ANOTHER task is not review evidence.
  stranger = await conftest.create_task(tree, parent=None, request_id="stranger")
  await tree.runs.register_run_locked(models.RunRecord(id="run-stranger-review", session_id=stranger.id, kind="review"))
  await tree.runs.record_finish(stranger.id, "run-stranger-review", "success")
  wrong_owner = task_completion.CompletionEvidence(
      summary="s",
      result_refs=["review:run-stranger-review"],
      run_ids=["run-work"],
      review_run_ids=["run-stranger-review"])
  blockers = tree.completion.evidence_blockers(index, index.metas[worker.id], wrong_owner)
  assert any("review Run of task" in b for b in blockers)

  # Automatic worker evidence closes the implement worker: a successful
  # review run of this task plus a commit that landed on its target branch.
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

  await tree.runs.register_run_locked(
      models.RunRecord(
          id="run-review", session_id=worker.id, kind="review", base_branch="main", review_of_run_id="run-work"))
  await tree.dispatch.finish_run(worker.id, "run-review", outcome="success")
  good = task_completion.CompletionEvidence(
      summary="implemented",
      result_refs=["run:run-work", f"spec:{'a' * 64}", f"landed:main@{landed_commit}"],
      run_ids=["run-work"],
      review_run_ids=["run-review"],
      landing=task_completion.LandingEvidence(branch="main", commit=landed_commit, repo_path=str(repo)))

  # A hash that exists nowhere is an explicit verified blocker.
  forged = task_completion.CompletionEvidence(
      summary="forged",
      result_refs=["run:run-work", f"landed:main@{'d' * 40}"],
      run_ids=["run-work"],
      review_run_ids=["run-review"],
      landing=task_completion.LandingEvidence(branch="main", commit="d" * 40, repo_path=str(repo)))
  blockers = await tree.completion.verified_evidence_blockers(index, index.metas[worker.id], forged)
  assert any("landing evidence unverified" in blocker for blocker in blockers)
  assert tree.task_state(worker.id) == "open"
  status, _payload = await tree.completion.evaluate_automatic_completion(
      worker.id, run_id="run-work", request_id="close-implement", evidence=good)
  assert status == 200
  assert tree.task_state(worker.id) == "completed"
  # The run records survive the close (evidence preserved).
  assert {r.id for r in tree.runs.list_run_records_sync(worker.id)} == {"run-work", "run-review", "run-misreview"}


# ---------------------------------------------------------------------------
# Own-manager close: 202 pending_run_finish
# ---------------------------------------------------------------------------


async def live_manager_caller(
    tree: task_sessions.TaskTreeManager, session_id: str, run_id: str) -> run_token.CallerIdentity:
  """Register one live manager_turn Run and return the agent caller its token binds."""
  pid, pid_start, started_at = live_identity()
  await tree.runs.register_run_locked(
      models.RunRecord(
          id=run_id, session_id=session_id, kind="manager_turn", pid=pid, pid_start=pid_start, started_at=started_at))
  return run_token.CallerIdentity(
      kind="agent", claims=run_token.RunTokenClaims(run_id=run_id, session_id=session_id, agent="manager"))


def close_requests_of(tree: task_sessions.TaskTreeManager, session_id: str) -> list[dict]:
  return [e for e in tree.events.load_events(session_id) if e["type"] == ET.TASK_CLOSE_REQUESTED]


@pytest.mark.asyncio
async def test_own_run_request_naming_a_foreign_run_is_refused_and_not_saved(tmp_path: pathlib.Path) -> None:
  """A run id that is not a Run of the task or its children (the 9/27 shape: a
  backend's own run id) is refused at request time instead of a 202 the
  re-evaluation could only block."""
  _cfg, _session_blocks, tree = conftest.build_env(tmp_path)
  root = await conftest.create_task(tree, parent=None, request_id="root")
  manager = await conftest.create_task(tree, parent=root.id, request_id="mgr")
  agent = await live_manager_caller(tree, manager.id, "run-mgr")

  evidence = task_completion.CompletionEvidence(
      summary="done", result_refs=["report:done"], run_ids=["run-mgr", "rollout-1"])
  with pytest.raises(task_errors.TaskConflictError) as refused:
    await tree.completion.archive_task(manager.id, request_id="close-1", evidence=evidence, caller=agent)
  assert any("rollout-1 is not a Run of task" in b for b in refused.value.blockers)
  # The caller's own still-active Run is no blocker: it is judged as the success it must become.
  assert not any("run-mgr" in b for b in refused.value.blockers)
  assert close_requests_of(tree, manager.id) == []
  assert tree.task_state(manager.id) == "open"


@pytest.mark.asyncio
async def test_own_run_request_with_an_open_child_is_refused_and_not_saved(tmp_path: pathlib.Path) -> None:
  _cfg, _session_blocks, tree = conftest.build_env(tmp_path)
  root = await conftest.create_task(tree, parent=None, request_id="root")
  manager = await conftest.create_task(tree, parent=root.id, request_id="mgr")
  child = await conftest.create_task(tree, parent=manager.id, request_id="child", profile="worker")
  agent = await live_manager_caller(tree, manager.id, "run-mgr")

  evidence = task_completion.CompletionEvidence(summary="done", result_refs=["report:done"], run_ids=["run-mgr"])
  with pytest.raises(task_errors.TaskConflictError) as refused:
    await tree.completion.archive_task(manager.id, request_id="close-1", evidence=evidence, caller=agent)
  assert f"has open descendant task {child.id}" in refused.value.blockers
  assert close_requests_of(tree, manager.id) == []
  assert tree.task_state(manager.id) == "open"


@pytest.mark.asyncio
async def test_saved_request_blocked_after_its_run_wakes_the_requester_once(tmp_path: pathlib.Path) -> None:
  """A valid request is saved; input arriving during the requesting Run blocks
  the close after that Run succeeds. The requester gets one wake input naming
  the request and its blockers — never a user event — and a recovery replay of
  the same request adds no second notice."""
  from src.runtime import task_recovery

  cfg, _session_blocks, tree = conftest.build_env(tmp_path)
  root = await conftest.create_task(tree, parent=None, request_id="root")
  manager = await conftest.create_task(tree, parent=root.id, request_id="mgr")
  agent = await live_manager_caller(tree, manager.id, "run-mgr")

  evidence = task_completion.CompletionEvidence(summary="done", result_refs=["report:done"], run_ids=["run-mgr"])
  status, payload = await tree.completion.archive_task(
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

  await task_recovery.reconcile_task_tree(cfg, tree)
  assert tree.task_state(manager.id) == "open"
  assert [e["id"] for e in tree.events.load_events(manager.id) if e["type"] == notice["type"]] == [notice["id"]]


@pytest.mark.asyncio
async def test_preexisting_close_request_closes_as_archived_and_reports_to_parent(tmp_path: pathlib.Path) -> None:
  _cfg, _session_blocks, tree = conftest.build_env(tmp_path)
  root = await conftest.create_task(tree, parent=None, request_id="root")
  manager = await conftest.create_task(tree, parent=root.id, request_id="manager")
  await live_manager_caller(tree, manager.id, "run-manager")
  request_id = "saved-before-archive-route"
  request = control_events.build_control_event(
      ET.TASK_CLOSE_REQUESTED,
      actor=control_events.ACTOR_SYSTEM,
      source_session_id=manager.id,
      event_id=control_events.stable_close_request_event_id(manager.id, request_id),
      request_id=request_id,
      owner_run_id="run-manager",
      summary="finished the task",
      result_refs=["report:finished"],
      run_ids=[])
  await tree.events.append(manager.id, request)
  tree._invalidate_index()

  await tree.dispatch.finish_run(manager.id, "run-manager", outcome="success")

  close = [e for e in tree.events.load_events(manager.id) if e["type"] == ET.TASK_CLOSED]
  reports = conftest.child_reports(tree, root.id)
  assert len(close) == 1 and close[0]["outcome"] == "archived"
  assert close[0]["report_to"] == root.id
  assert len(reports) == 1 and reports[0]["outcome"] == "archived"


# ---------------------------------------------------------------------------
# Cancel and restore
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_agent_cancels_only_its_own_direct_child(tmp_path: pathlib.Path) -> None:
  """An agent cancels a direct child of its own task (the close fact records
  the agent as the actor); a sibling, grandchild, and unrelated root with
  live Runs remain untouched by forbidden requests."""
  _cfg, _session_blocks, tree = conftest.build_env(tmp_path)
  root = await conftest.create_task(tree, parent=None, request_id="root")
  manager = await conftest.create_task(tree, parent=root.id, request_id="mgr")
  sibling = await conftest.create_task(tree, parent=root.id, request_id="sib", profile="worker")
  worker = await conftest.create_task(tree, parent=manager.id, request_id="worker", profile="worker")
  # The grandchild shape: a logical-manager child of the agent's own task with
  # a child of its own — two levels below the agent's task, never a direct one.
  sub = await conftest.create_task(tree, parent=manager.id, request_id="sub")
  grandchild = await conftest.create_task(tree, parent=sub.id, request_id="grand", profile="worker")
  unrelated = await conftest.create_task(tree, parent=None, request_id="unrelated", profile="worker")
  agent = run_token.CallerIdentity(
      kind="agent", claims=run_token.RunTokenClaims(run_id="run-mgr", session_id=manager.id, agent="manager-agent"))

  processes = {
      sibling.id: await register_isolated_sleep_run(tree, sibling.id, "run-sibling"),
      grandchild.id: await register_isolated_sleep_run(tree, grandchild.id, "run-grandchild"),
      unrelated.id: await register_isolated_sleep_run(tree, unrelated.id, "run-unrelated"),
  }
  try:
    # Out of scope: grandchild, sibling, parent, self, and an unrelated root.
    for target, request_id in ((grandchild, "cancel-g"), (sibling, "cancel-s"), (root, "cancel-p"),
                               (manager, "cancel-self"), (unrelated, "cancel-u")):
      with pytest.raises(task_errors.TaskForbiddenError, match="direct child"):
        await tree.completion.cancel_task(target.id, request_id=request_id, reason="x", caller=agent)
      if target.id in processes:
        assert processes[target.id].poll() is None

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

    # A queued agent message stays on the cancelled worker and cannot start a
    # replacement Run after the live Run stops.
    worker2 = await conftest.create_task(tree, parent=manager.id, request_id="worker2", profile="worker")
    proc = await register_isolated_sleep_run(tree, worker2.id, "run-w2")
    try:
      await tree.dispatch.admit_input(root.id, event_type=ET.USER, content="user request", actor="user")
      await tree.dispatch.admit_input(
          worker2.id,
          event_type=ET.AGENT_MESSAGE,
          content="continue after this run",
          actor="agent",
          from_session=manager.id)
      result = await tree.completion.cancel_task(worker2.id, request_id="cancel-w2", reason="x", caller=agent)
      assert proc.poll() is not None
      assert tree.task_state(worker2.id) == "cancelled"
      assert result["stopped_runs"] == [{"run_id": "run-w2", "outcome": "interrupted"}]
      assert len(tree.runs.list_run_records_sync(worker2.id)) == 1
      reports = [
          e for e in tree.events.load_events(manager.id)
          if e["type"] == ET.CHILD_REPORT and e["child_session_id"] == worker2.id
      ]
      assert len(reports) == 1
    finally:
      if proc.poll() is None:
        proc.kill()
      proc.wait(timeout=1)
  finally:
    for proc in processes.values():
      if proc.poll() is None:
        proc.kill()
      proc.wait(timeout=1)


@pytest.mark.asyncio
async def test_cancel_stops_a_queued_worker_run_without_launching_it(tmp_path: pathlib.Path) -> None:
  """A queued worker Run is stopped by the cancellation and never launches."""
  _cfg, _session_blocks, tree = conftest.build_env(tmp_path)
  root = await conftest.create_task(tree, parent=None, request_id="root")
  child = await conftest.create_task(tree, parent=root.id, request_id="child", profile="worker")
  await tree.runs.register_run_locked(models.RunRecord(id="run-queued", session_id=child.id, kind="work"))

  result = await tree.completion.cancel_task(
      child.id, request_id="cancel-1", reason="stopped work is settled", caller=conftest.OPERATOR)
  repeated = await tree.completion.cancel_task(
      child.id, request_id="cancel-1", reason="stopped work is settled", caller=conftest.OPERATOR)
  close = [e for e in tree.events.load_events(child.id) if e["type"] == ET.TASK_CLOSED]
  assert len(close) == 1 and close[0]["outcome"] == "cancelled"
  assert result == repeated
  assert result["stopped_runs"] == [{"run_id": "run-queued", "outcome": "never_launched"}]
  stops = [e for e in tree.events.load_events(child.id) if e["type"] == ET.RUN_STOP_REQUESTED]
  assert len(stops) == 1 and stops[0]["request_id"] == "cancel:cancel-1"
  # The queued run keeps its no-terminal-fact shape: settled by request, not
  # by an outcome.
  runs = tree.runs.list_run_records_sync(child.id)
  assert len(runs) == 1 and runs[0].pid is None
  assert tree.runs.terminal_outcome(tree.runs.load_events_sync(child.id), "run-queued") is None


@pytest.mark.asyncio
async def test_cancel_of_manager_with_a_live_run_keeps_the_run_alive(tmp_path: pathlib.Path) -> None:
  _cfg, _session_blocks, tree = conftest.build_env(tmp_path)
  parent = await conftest.create_task(tree, parent=None, request_id="manager-parent")
  manager = await conftest.create_task(tree, parent=parent.id, request_id="manager-child", profile="manager")
  proc = await register_isolated_sleep_run(tree, manager.id, "run-manager")
  try:
    with pytest.raises(task_errors.TaskConflictError, match="active"):
      await tree.completion.cancel_task(
          manager.id, request_id="cancel-manager", reason="stop", caller=conftest.OPERATOR)
    assert proc.poll() is None
    assert tree.task_state(manager.id) == "open"
  finally:
    if proc.poll() is None:
      proc.kill()
    proc.wait(timeout=1)


@pytest.mark.asyncio
async def test_cancel_refuses_a_live_sequence_run_without_writing_facts(tmp_path: pathlib.Path) -> None:
  _cfg, _session_blocks, tree = conftest.build_env(tmp_path)
  parent = await conftest.create_task(tree, parent=None, request_id="sequence-parent")
  worker = await conftest.create_task(tree, parent=parent.id, request_id="sequence-worker", profile="worker")
  proc = await register_isolated_sleep_run(
      tree,
      worker.id,
      "run-sequence",
      sequence_ref=models.SequenceRef(kind="improve", owner_ref="improve:one", position=0))
  before = tree.events.load_events(worker.id)
  try:
    with pytest.raises(task_errors.TaskConflictError, match="sequence controller"):
      await tree.completion.cancel_task(
          worker.id, request_id="cancel-sequence", reason="stop", caller=conftest.OPERATOR)
    assert proc.poll() is None
    assert tree.events.load_events(worker.id) == before
    assert tree.task_state(worker.id) == "open"
  finally:
    if proc.poll() is None:
      proc.kill()
    proc.wait(timeout=1)


@pytest.mark.asyncio
async def test_failure_report_reaching_lock_after_cancel_is_skipped(tmp_path: pathlib.Path) -> None:
  cfg, session_blocks, tree = conftest.build_env(tmp_path)
  parent = await conftest.create_task(tree, parent=None, request_id="failure-parent")
  worker = await conftest.create_task(tree, parent=parent.id, request_id="failure-worker", profile="worker")
  await tree.runs.register_run_locked(models.RunRecord(id="run-failed", session_id=worker.id, kind="work"))
  await tree.runs.record_finish(worker.id, "run-failed", "failed")
  await tree.completion.cancel_task(
      worker.id, request_id="cancel-before-report", reason="stop", caller=conftest.OPERATOR)
  report_count = len(conftest.child_reports(tree, parent.id))
  archive_calls: list[str] = []

  async def record_archive(session_id: str, *, run_id: str, outcome: str) -> bool:
    archive_calls.append(session_id)
    return False

  tree.completion.archive_unsuccessful_worker = record_archive
  adapter = conftest.build_execution_adapter(cfg, session_blocks, tree)
  run = await tree.runs.get_run(worker.id, "run-failed")
  assert run is not None
  await adapter._report_failure_to_parent(worker.id, run, "failed", summary="late failure")

  reports = conftest.child_reports(tree, parent.id)
  assert len(reports) == report_count == 1
  assert archive_calls == []


@pytest.mark.asyncio
async def test_successful_work_on_closed_worker_does_not_register_review(tmp_path: pathlib.Path) -> None:
  cfg, session_blocks, tree = conftest.build_env(tmp_path)
  parent = await conftest.create_task(tree, parent=None, request_id="review-parent")
  worker = await conftest.create_task(
      tree,
      parent=parent.id,
      request_id="review-worker",
      profile="worker",
      task=models.TaskSpec(task_type=models.TaskType.IMPLEMENT))
  await tree.runs.register_run_locked(models.RunRecord(id="run-work", session_id=worker.id, kind="work"))
  await tree.runs.record_finish(worker.id, "run-work", "success")
  await tree.completion.cancel_task(
      worker.id, request_id="cancel-before-review", reason="stop", caller=conftest.OPERATOR)
  adapter = conftest.build_execution_adapter(cfg, session_blocks, tree)
  tree.dispatch.executor = adapter
  meta = await tree.load_task_meta(worker.id)
  run = await tree.runs.get_run(worker.id, "run-work")
  await adapter._after_worker_run(meta, run, "success")

  run_records = tree.runs.list_run_records_sync(worker.id)
  reports = conftest.child_reports(tree, parent.id)
  assert [record.id for record in run_records] == ["run-work"]
  assert len(reports) == 1


# ---------------------------------------------------------------------------
# Who closed the gate: the parent's own turn skips the wake, others wake it
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_cancel_by_the_manager_parent_records_close_and_child_report(tmp_path: pathlib.Path) -> None:
  """Cancelling a worker preserves its close fact and reports the result to its manager node."""
  _cfg, _session_blocks, tree = conftest.build_env(tmp_path)
  manager = await conftest.create_task(tree, parent=None, request_id="manager")
  child = await conftest.create_task(tree, parent=manager.id, request_id="child", profile="worker")
  payload = await tree.completion.cancel_task(
      child.id,
      request_id="cancel-1",
      reason="no longer needed",
      caller=run_token.CallerIdentity(kind="operator", session_id=manager.id))

  assert payload["closed_event_id"]
  closes, reports = persisted_close_and_report(tree, manager.id, child.id, "cancelled")
  assert len(closes) == 1 and closes[0]["summary"] == "no longer needed"
  assert len(reports) == 1 and reports[0]["outcome"] == "cancelled"


# ---------------------------------------------------------------------------
# close_sequence_child: the sequence owner's close of an ended loop's child
# ---------------------------------------------------------------------------


async def _sequence_child(tree: task_sessions.TaskTreeManager) -> tuple[object, object]:
  """One root with one open worker child, the shape a sequence close targets."""
  root = await conftest.create_task(tree, parent=None, request_id="root")
  child = await conftest.create_task(
      tree, parent=root.id, request_id="child", profile="worker", task=models.TaskSpec(goal="loop"))
  return root, child


def _sequence_close_and_reports(tree: task_sessions.TaskTreeManager, root_id: str,
                                child_id: str) -> tuple[list[dict], list[dict]]:
  """The child's task_closed facts and the parent's child_reports from it."""
  closes = [e for e in tree.events.load_events(child_id) if e["type"] == ET.TASK_CLOSED]
  reports = [e for e in conftest.child_reports(tree, root_id) if e["child_session_id"] == child_id]
  return closes, reports


@pytest.mark.asyncio
async def test_close_sequence_child_completed_on_proven_run_evidence(tmp_path: pathlib.Path) -> None:
  """A completed loop outcome with a successful named Run closes completed: the
  close fact carries the loop outcome as sequence_outcome, records the system
  actor, and the one delivered report takes the sequence outcome."""
  _cfg, _session_blocks, tree = conftest.build_env(tmp_path)
  root, child = await _sequence_child(tree)
  await tree.runs.register_run_locked(models.RunRecord(id="run-iter-1", session_id=child.id, kind="iteration"))
  await tree.dispatch.finish_run(child.id, "run-iter-1", outcome="success")

  evidence = task_completion.CompletionEvidence(
      summary="[Improve loop 1] landed", result_refs=["loop:1"], run_ids=["run-iter-1"])
  status, payload = await tree.completion.close_sequence_child(
      child.id, request_id="improve:1:close", outcome="completed", evidence=evidence)

  assert status == 200
  assert tree.task_state(child.id) == "completed"
  closes, reports = _sequence_close_and_reports(tree, root.id, child.id)
  assert len(closes) == 1 and len(reports) == 1
  close = closes[0]
  assert close["id"] == payload["closed_event_id"]
  assert close["actor"] == "system"
  assert close["outcome"] == "completed" and close["sequence_outcome"] == "completed"
  assert close["run_ids"] == ["run-iter-1"]
  assert reports[0]["outcome"] == "completed"
  assert reports[0]["summary"] == "[Improve loop 1] landed"
  assert reports[0]["result_refs"] == ["loop:1"]


@pytest.mark.asyncio
async def test_close_sequence_child_completed_without_a_successful_run_closes_cancelled(tmp_path: pathlib.Path) -> None:
  """The completed loop outcome without a named successful Run writes the
  cancelled lifecycle (an empty-handed delivery is never completed) while the
  report still tells the parent the loop completed."""
  _cfg, _session_blocks, tree = conftest.build_env(tmp_path)
  root, child = await _sequence_child(tree)

  evidence = task_completion.CompletionEvidence(summary="[Improve loop 1] landed", result_refs=["loop:1"], run_ids=[])
  status, _payload = await tree.completion.close_sequence_child(
      child.id, request_id="improve:1:close", outcome="completed", evidence=evidence)

  assert status == 200
  assert tree.task_state(child.id) == "cancelled"
  closes, reports = _sequence_close_and_reports(tree, root.id, child.id)
  assert closes[0]["outcome"] == "cancelled" and closes[0]["sequence_outcome"] == "completed"
  assert reports[0]["outcome"] == "completed"


@pytest.mark.asyncio
@pytest.mark.parametrize("outcome", ["blocked", "failed", "cancelled"])
async def test_close_sequence_child_non_completed_outcomes_record_evidence_as_content(
    tmp_path: pathlib.Path, outcome: str) -> None:
  """blocked/failed/cancelled close as cancelled with the evidence carried as
  content only: a failed Run named in run_ids is never read as completion
  evidence, and the report outcome is the loop's own."""
  _cfg, _session_blocks, tree = conftest.build_env(tmp_path)
  root, child = await _sequence_child(tree)
  await tree.runs.register_run_locked(models.RunRecord(id="run-iter-1", session_id=child.id, kind="iteration"))
  await tree.dispatch.finish_run(child.id, "run-iter-1", outcome="failed")

  evidence = task_completion.CompletionEvidence(
      summary=f"[Improve loop 1] {outcome}", result_refs=["loop:1"], run_ids=["run-iter-1"])
  status, _payload = await tree.completion.close_sequence_child(
      child.id, request_id="improve:1:close", outcome=outcome, evidence=evidence)

  assert status == 200
  assert tree.task_state(child.id) == "cancelled"
  closes, reports = _sequence_close_and_reports(tree, root.id, child.id)
  assert len(closes) == 1 and len(reports) == 1
  assert closes[0]["outcome"] == "cancelled" and closes[0]["sequence_outcome"] == outcome
  assert closes[0]["run_ids"] == ["run-iter-1"]  # content, not a success claim
  assert reports[0]["outcome"] == outcome


@pytest.mark.asyncio
async def test_close_sequence_child_blocks_on_execution_only(tmp_path: pathlib.Path) -> None:
  """The cancellation blocker rule: a queued Run without a stop request blocks
  (an active Run and an open descendant read the same way); unprocessed input
  never does."""
  _cfg, _session_blocks, tree = conftest.build_env(tmp_path)
  _root, child = await _sequence_child(tree)
  await tree.runs.register_run_locked(models.RunRecord(id="run-queued", session_id=child.id, kind="iteration"))
  await tree.dispatch.admit_input(child.id, event_type=ET.USER, content="unconsumed note", actor="user")
  evidence = task_completion.CompletionEvidence(summary="[Improve loop 1] blocked", result_refs=["loop:1"])

  with pytest.raises(task_errors.TaskConflictError, match="queued"):
    await tree.completion.close_sequence_child(
        child.id, request_id="improve:1:close", outcome="blocked", evidence=evidence)

  # The durable stop request settles the queued Run; the unprocessed input
  # stays pending on the child and never blocks the close.
  await tree.runs.request_stop(child.id, "run-queued", "improve:1:close")
  assert tree.dispatch.pending_inputs(child.id) != []
  status, _payload = await tree.completion.close_sequence_child(
      child.id, request_id="improve:1:close", outcome="blocked", evidence=evidence)
  assert status == 200
  assert tree.task_state(child.id) == "cancelled"


@pytest.mark.asyncio
async def test_close_sequence_child_replays_raced_repeated_and_post_reopen_calls(tmp_path: pathlib.Path) -> None:
  """One request id lands one close fact and one report: two raced calls
  converge, a repeated call replays the first result, and a call arriving
  after the operator reopened the node replays it too (the node stays open).
  """
  _cfg, _session_blocks, tree = conftest.build_env(tmp_path)
  root, child = await _sequence_child(tree)
  evidence = task_completion.CompletionEvidence(summary="[Improve loop 1] blocked", result_refs=["loop:1"])

  first, second = await asyncio.gather(
      tree.completion.close_sequence_child(
          child.id, request_id="improve:1:close", outcome="blocked", evidence=evidence),
      tree.completion.close_sequence_child(
          child.id, request_id="improve:1:close", outcome="blocked", evidence=evidence))
  assert first[0] == second[0] == 200
  assert first[1]["closed_event_id"] == second[1]["closed_event_id"]
  closes, reports = _sequence_close_and_reports(tree, root.id, child.id)
  assert len(closes) == 1 and len(reports) == 1

  repeated = await tree.completion.close_sequence_child(
      child.id,
      request_id="improve:1:close",
      outcome="failed",
      evidence=task_completion.CompletionEvidence(summary="other"))
  assert repeated[1]["closed_event_id"] == first[1]["closed_event_id"]
  closes, reports = _sequence_close_and_reports(tree, root.id, child.id)
  assert len(closes) == 1 and len(reports) == 1

  await tree.completion.restore_task(child.id, request_id="reopen-1", reason="operator retry", caller=conftest.OPERATOR)
  assert tree.task_state(child.id) == "open"
  after_reopen = await tree.completion.close_sequence_child(
      child.id, request_id="improve:1:close", outcome="blocked", evidence=evidence)
  assert after_reopen[1]["closed_event_id"] == first[1]["closed_event_id"]
  assert tree.task_state(child.id) == "open"
  closes, reports = _sequence_close_and_reports(tree, root.id, child.id)
  assert len(closes) == 1 and len(reports) == 1


@pytest.mark.asyncio
async def test_recovered_sequence_close_report_takes_the_sequence_outcome(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The crash window close-fact-written/report-not-delivered: recovery
  re-derives the report from the close fact and its outcome is the fact's
  sequence_outcome, not its lifecycle outcome."""
  _cfg, _session_blocks, tree = conftest.build_env(tmp_path)
  root, child = await _sequence_child(tree)

  original = tree.dispatch.deliver_child_report_locked

  async def crash_before_report(*args, **kwargs):
    raise RuntimeError("simulated crash after the close fact")

  monkeypatch.setattr(tree.dispatch, "deliver_child_report_locked", crash_before_report)
  evidence = task_completion.CompletionEvidence(summary="[Improve loop 1] blocked", result_refs=["loop:1"])
  with pytest.raises(RuntimeError, match="simulated crash"):
    await tree.completion.close_sequence_child(
        child.id, request_id="improve:1:close", outcome="blocked", evidence=evidence)
  closes, reports = _sequence_close_and_reports(tree, root.id, child.id)
  assert len(closes) == 1 and closes[0]["outcome"] == "cancelled" and closes[0]["sequence_outcome"] == "blocked"
  assert reports == []

  monkeypatch.setattr(tree.dispatch, "deliver_child_report_locked", original)
  delivered = await tree.dispatch.recover_pending_reports(child.id)
  assert len(delivered) == 1
  assert delivered[0]["outcome"] == "blocked"
  assert delivered[0]["summary"] == "[Improve loop 1] blocked"
