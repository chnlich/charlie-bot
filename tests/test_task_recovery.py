"""Startup-recovery tests: the v2 reconciliation pass converges every node.

Simulates restarts at the reservation, launch-identity, stop-request, terminal
append, review-creation, and parent-delivery boundaries, then runs the actual
startup pass (src.runtime.task_recovery.reconcile_task_tree) and asserts exact
input ownership, no duplicate processes/side effects/reports, repaired missing
follow-ups, and idempotence under a repeated pass. A second synthetic
instance's data and owned processes stay untouched.
"""

from __future__ import annotations

import asyncio
import datetime
import functools
import hashlib
import json
import pathlib

import conftest
import pytest

import src.runtime.task_execution as task_execution_module
from src.infra import event_types as ET
from src.infra import models, ndjson
from src.runtime import control_events, runs, task_recovery
from tests import test_task_execution


async def _takeoff_manager(tmp_path, monkeypatch):
  """One manager with its take-off turn already consumed (scripted backend)."""
  cfg, session_blocks, tree = test_task_execution.build_env(tmp_path, monkeypatch)
  tree.dispatch.executor = test_task_execution._adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  conftest.install_scripted_backends(
      monkeypatch, [test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("taken off")])],
      conftest.BUILD_BACKEND_PATCH_TARGET)
  manager = await tree.create_task(
      request_id="root",
      task_parent_id=None,
      profile="manager",
      task=models.TaskSpec(goal="pm"),
      name="PM",
      backend=None,
      caller="operator")
  await tree.dispatch.admit_input(manager.id, event_type=ET.USER, content="Take off. Do the work.", actor="user")
  decision = await tree.dispatch.dispatch_pending(manager.id)
  assert decision["launch"] is True
  await test_task_execution.wait_for_terminal_run(tree, manager.id, decision["run_id"])
  return cfg, session_blocks, tree, manager


async def _manager_and_worker(tmp_path, monkeypatch, task_type=None, repo=None):
  cfg, session_blocks, tree, manager = await _takeoff_manager(tmp_path, monkeypatch)
  child_task = models.TaskSpec(goal="do the work", repo_path=repo, task_type=task_type)
  worker = await tree.create_task(
      request_id="child",
      task_parent_id=manager.id,
      profile="worker",
      task=child_task,
      name="W",
      backend=None,
      caller="operator")
  return cfg, session_blocks, tree, manager, worker


@pytest.mark.asyncio
async def test_restart_before_launch_requeues_through_the_same_launch_checks(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A run registered (reservation) but never launched: recovery dispatches it
    through the executor — one process, the exact registered provenance."""
  cfg, _session_blocks, tree, _manager, worker = await _manager_and_worker(tmp_path, monkeypatch)
  backend = test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("recovered work")])
  builds = test_task_execution.install_worker_launch_and_resume_backends(monkeypatch, [backend])
  admitted = await tree.dispatch.admit_input(worker.id, event_type=ET.USER, content="Start the task.", actor="user")
  # Simulate the crash window: the reservation exists (batch claimed) but the
  # launch never happened — no pid, no terminal fact.
  run_id = "run-queued"
  await tree.runs.register_run_locked(
      models.RunRecord(id=run_id, session_id=worker.id, kind="work", backend="fake", model="fake-model"))
  await tree.dispatch.claim_input_batch_locked(worker.id, run_id)

  await task_recovery.reconcile_task_tree(cfg, tree)
  run, outcome = await test_task_execution.wait_for_terminal_run(tree, worker.id, run_id)
  assert outcome == "success"
  assert run.pid == 424001
  assert run.input_event_ids == [str(admitted["id"])]
  assert len(builds) == 1
  # A repeated pass launches nothing new and duplicates nothing.
  await task_recovery.reconcile_task_tree(cfg, tree)
  assert len(tree.runs.list_run_records_sync(worker.id)) == 1
  assert len(builds) == 1


@pytest.mark.asyncio
async def test_restart_after_launch_reattaches_live_process(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A recorded live (pid, pid_start) process is followed, never relaunched."""
  cfg, _session_blocks, tree, _manager, worker = await _manager_and_worker(tmp_path, monkeypatch)
  gate = asyncio.Event()
  backend = test_task_execution.SpawningScriptedBackend(
      [test_task_execution.result_event("late result")], gate=gate.wait)
  builds = test_task_execution.install_worker_launch_and_resume_backends(monkeypatch, [backend])
  run_id = "run-live"
  await tree.runs.register_run_locked(
      models.RunRecord(id=run_id, session_id=worker.id, kind="work", backend="fake", model="fake-model"))
  # The crash window: launched and recorded, no terminal fact.
  await tree.runs.record_launch(worker.id, run_id, pid=424777, pid_start="1-424000")
  # The backend double's fake pid does not exist on this host: to exercise the
  # FOLLOW path, stub the liveness judgment — alive while the process works,
  # ended once the gate releases (the true→false transition the follower must
  # observe instead of a captured boolean).
  import src.runtime.runs as runs_mod
  monkeypatch.setattr(runs_mod, "is_run_alive", lambda *a, **k: not gate.is_set())

  async def _release_soon():
    await asyncio.sleep(0.3)
    gate.set()

  asyncio.get_event_loop().create_task(_release_soon())
  await task_recovery.reconcile_task_tree(cfg, tree)
  _run, outcome = await test_task_execution.wait_for_terminal_run(tree, worker.id, run_id)
  # The follow observed the recorded process ENDING (true→false) and the run
  # converged to its durable result instead of staying falsely running. The
  # scripted double never wrote a raw stream (no launcher drove it), so the
  # honest outcome is failed — never a relaunch, never a fabricated success.
  assert outcome in ("failed", "interrupted")
  assert len(builds) == 0
  assert len(tree.runs.list_run_records_sync(worker.id)) == 1


@pytest.mark.asyncio
async def test_stop_request_wins_over_launch_and_recovery(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, _session_blocks, tree, _manager, worker = await _manager_and_worker(tmp_path, monkeypatch)
  builds = conftest.install_scripted_backends(
      monkeypatch, [test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("x")])],
      conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)
  run_id = "run-stopped"
  await tree.runs.register_run_locked(
      models.RunRecord(id=run_id, session_id=worker.id, kind="work", backend="fake", model="fake-model"))
  await tree.dispatch.claim_input_batch_locked(worker.id, run_id)
  await tree.runs.request_stop(worker.id, run_id, "stop-1")
  await task_recovery.reconcile_task_tree(cfg, tree)
  # Stop precedence: the queued run never launches and never claims input;
  # the durable stop request stands and no side effect happened.
  events = tree.runs.load_events_sync(worker.id)
  assert tree.runs.stop_requested(events, run_id)
  assert builds == []
  stopped_run = await tree.runs.get_run(worker.id, run_id)
  assert stopped_run is not None and stopped_run.pid is None
  await task_recovery.reconcile_task_tree(cfg, tree)
  assert builds == []


@pytest.mark.asyncio
async def test_recovery_never_rereviews_a_successfully_reviewed_work_run(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A work run whose review already succeeded never gets another review:
    restart recovery any number of times registers none (the chain ends at
    the first successful review), while the review's own follow-up replay
    (landing recheck) stays idempotent."""
  cfg, _session_blocks, tree, _manager, worker = await _manager_and_worker(tmp_path, monkeypatch)
  repo, _origin = conftest.init_repo_with_origin(tmp_path / "repo")
  await tree.patch_task(
      worker.id,
      models.PatchSessionTaskRequest(
          task=models.TaskSpec(goal="do the work", repo_path=str(repo), task_type=models.TaskType.IMPLEMENT)),
      caller=conftest.OPERATOR)
  conftest.install_scripted_backends(
      monkeypatch, [test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("review ok")])],
      conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)
  run_id = "run-reviewed"
  await tree.runs.register_run_locked(
      models.RunRecord(
          id=run_id,
          session_id=worker.id,
          kind="work",
          backend="fake",
          model="fake-model",
          repo_path=str(repo),
          base_branch="main",
          branch_name="task/work",
          worktree_path=str(repo)))
  await tree.dispatch.finish_run(worker.id, run_id, outcome="success", exit_code=0)
  # The review ran to its own successful terminal fact (the state every
  # server restart used to re-enter with a fresh reviewer backend).
  review_id = "review-done"
  await tree.runs.register_run_locked(
      models.RunRecord(
          id=review_id,
          session_id=worker.id,
          kind="review",
          review_of_run_id=run_id,
          backend="fake",
          model="fake-model"))
  await tree.dispatch.finish_run(worker.id, review_id, outcome="success", exit_code=0)

  for _round in range(2):
    await task_recovery.reconcile_task_tree(cfg, tree)
    reviews = [r for r in tree.runs.list_run_records_sync(worker.id) if r.kind == "review"]
    assert len(reviews) == 1, f"recovery registered a new review: {[r.id for r in reviews]}"
    assert reviews[0].id == review_id


async def _implement_worker_with_landed_branch(tmp_path, monkeypatch):
  """One implement worker on a repo whose reviewed branch already landed on
    origin's base, the worker backend doubles and the parent's report-turn build
    installed: the durable shape a restart reconciles, awaiting the test's run
    records."""

  cfg, session_blocks, tree, manager, worker = await _manager_and_worker(
      tmp_path, monkeypatch, task_type=models.TaskType.IMPLEMENT)
  repo, _origin = conftest.init_repo_with_origin(tmp_path / "repo")
  # The reviewer's landing: the reviewed branch fast-forwards origin's base.
  conftest.run_git(repo, "checkout", "-q", "-b", "task/work")
  (repo / "marker.txt").write_text("implemented\n")
  conftest.run_git(repo, "add", "-A")
  conftest.run_git(repo, "commit", "-q", "-m", "implement marker")
  conftest.run_git(repo, "push", "-q", "origin", "task/work:main")
  conftest.install_scripted_backends(
      monkeypatch, [
          test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("review ok")]),
          test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("review ok again")])
      ], conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)
  # The delivered report's parent turn: one fresh scripted double per build.
  monkeypatch.setattr(conftest.BUILD_BACKEND_PATCH_TARGET, test_task_execution.make_pm_build("report consumed"))
  return cfg, session_blocks, tree, manager, worker, repo


async def _register_successful_work_run(tree, worker_id, run_id, repo, *, started_at, worktree_path):
  """One work run in its delivered durable shape: launched at *started_at*,
    finished success, never reviewed."""
  await tree.runs.register_run_locked(
      models.RunRecord(
          id=run_id,
          session_id=worker_id,
          kind="work",
          backend="fake",
          model="fake-model",
          repo_path=str(repo),
          base_branch="main",
          branch_name="task/work",
          worktree_path=worktree_path,
          started_at=started_at))
  await tree.dispatch.finish_run(worker_id, run_id, outcome="success", exit_code=0)


@pytest.mark.asyncio
async def test_recovery_spawns_no_review_for_a_superseded_work_run_on_a_closed_task(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A completed implement worker whose earlier work run a later one superseded
    before any review: the startup replay registers no review for it (the live
    path read the later run as a pause), however many times recovery runs."""
  cfg, _session_blocks, tree, manager, worker, repo = await _implement_worker_with_landed_branch(tmp_path, monkeypatch)
  worktree_gone = str(tmp_path / "charliebot-home" / "worktrees" / "task-work-gone")
  started = datetime.datetime.now(datetime.UTC) - datetime.timedelta(hours=1)
  await _register_successful_work_run(
      tree, worker.id, "run-work-early", repo, started_at=started, worktree_path=worktree_gone)
  await _register_successful_work_run(
      tree,
      worker.id,
      "run-work-late",
      repo,
      started_at=started + datetime.timedelta(minutes=30),
      worktree_path=worktree_gone)
  review_id = "review-done"
  await tree.runs.register_run_locked(
      models.RunRecord(
          id=review_id,
          session_id=worker.id,
          kind="review",
          review_of_run_id="run-work-late",
          backend="fake",
          model="fake-model",
          started_at=started + datetime.timedelta(minutes=45)))
  await tree.dispatch.finish_run(worker.id, review_id, outcome="success", exit_code=0)

  # The first pass replays the review's landing proof and closes the task; no
  # pass ever registers a review for the superseded run.
  for _round in range(2):
    await task_recovery.reconcile_task_tree(cfg, tree)
    reviews = [r for r in tree.runs.list_run_records_sync(worker.id) if r.kind == "review"]
    assert [r.id for r in reviews] == [review_id], f"recovery registered a new review: {[r.id for r in reviews]}"
  assert tree.task_state(worker.id) == "completed"
  await conftest._settle_parent(tree, manager, timeout=5.0, poll=0.02)


@pytest.mark.asyncio
async def test_recovery_reviews_only_the_newest_unreviewed_work_run_on_an_open_task(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """An open idle implement worker: the replay spawns no review for an earlier
    work run a later one superseded, and exactly one for the newest successful
    work run that has none — the one the live path would have reviewed."""
  cfg, _session_blocks, tree, manager, worker, repo = await _implement_worker_with_landed_branch(tmp_path, monkeypatch)
  # A second member for the reviewer: it never runs on the worker's member.
  cfg.backends.options.append(conftest.backend_option(id="fake2", label="Fake2", type="codex", model="fake2-model"))
  cfg.backends.pool = ["fake", "fake2"]
  started = datetime.datetime.now(datetime.UTC) - datetime.timedelta(hours=1)
  # The spawned review launches in the work run's worktree: an existing path.
  await _register_successful_work_run(
      tree, worker.id, "run-work-early", repo, started_at=started, worktree_path=str(repo))
  await _register_successful_work_run(
      tree,
      worker.id,
      "run-work-late",
      repo,
      started_at=started + datetime.timedelta(minutes=30),
      worktree_path=str(repo))

  await task_recovery.reconcile_task_tree(cfg, tree)

  reviews = [r for r in tree.runs.list_run_records_sync(worker.id) if r.kind == "review"]
  assert len(reviews) == 1, f"recovery registered {[r.id for r in reviews]}"
  assert reviews[0].review_of_run_id == "run-work-late"
  # The spawned review runs to its own success and delivers; a repeated pass
  # registers nothing more.
  _run, outcome = await test_task_execution.wait_for_terminal_run(tree, worker.id, reviews[0].id)
  assert outcome == "success"
  await conftest._settle_parent(tree, manager, timeout=5.0, poll=0.02)
  await task_recovery.reconcile_task_tree(cfg, tree)
  reviews = [r for r in tree.runs.list_run_records_sync(worker.id) if r.kind == "review"]
  assert len(reviews) == 1, f"recovery registered a new review: {[r.id for r in reviews]}"


@pytest.mark.asyncio
async def test_recovery_reports_blocked_when_the_pool_has_no_reviewer_member(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A single-member pool can never staff the reviewer (it never uses the worker's member): the
    startup replay of a successful unreviewed work run reports blocked instead of hanging open."""
  cfg, _session_blocks, tree, manager, worker, repo = await _implement_worker_with_landed_branch(tmp_path, monkeypatch)
  started = datetime.datetime.now(datetime.UTC) - datetime.timedelta(hours=1)
  await _register_successful_work_run(tree, worker.id, "run-work", repo, started_at=started, worktree_path=str(repo))

  await task_recovery.reconcile_task_tree(cfg, tree)

  assert [r for r in tree.runs.list_run_records_sync(worker.id) if r.kind == "review"] == []
  reports = [e for e in tree.events.load_events(manager.id) if e["type"] == ET.CHILD_REPORT]
  assert len(reports) == 1 and reports[0]["outcome"] == "blocked"
  assert "found no available reviewer backend" in reports[0]["summary"]
  await test_task_execution.poll_until(
      lambda: tree.task_state(worker.id) == "archived", timeout=15.0, what="the blocked worker's archive")
  # A repeated pass lands nothing twice.
  await task_recovery.reconcile_task_tree(cfg, tree)
  assert [e for e in tree.events.load_events(manager.id) if e["type"] == ET.CHILD_REPORT] == reports


@pytest.mark.asyncio
async def test_recovery_spawns_no_review_behind_a_registered_unstarted_work_run(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """An open idle implement worker whose newest work run is registered but
    never started: the replay spawns no review for the earlier successful run
    (the live path read the reservation as a pause), and the queued run's own
    withheld launch on an open node keeps its shape — queued, no stop
    request."""
  cfg, _session_blocks, tree, manager, worker, repo = await _implement_worker_with_landed_branch(tmp_path, monkeypatch)
  cfg.server.min_free_disk_gib = 10**9  # no filesystem holds this much: the queued run's launch is withheld
  started = datetime.datetime.now(datetime.UTC) - datetime.timedelta(hours=1)
  await _register_successful_work_run(
      tree, worker.id, "run-work-early", repo, started_at=started, worktree_path=str(repo))
  queued_id = "run-work-queued"
  await tree.runs.register_run_locked(
      models.RunRecord(id=queued_id, session_id=worker.id, kind="work", backend="fake", model="fake-model"))

  await task_recovery.reconcile_task_tree(cfg, tree)

  reviews = [r for r in tree.runs.list_run_records_sync(worker.id) if r.kind == "review"]
  assert reviews == [], f"recovery registered a review: {[r.id for r in reviews]}"
  # The queued run's dispatch was withheld for disk headroom: an open node's
  # withhold records no stop request, and the run stays queued.
  await test_task_execution.poll_until(
      lambda: any(
          e.get("type") == ET.RUN_LAUNCH_WITHHELD and e.get("run_id") == queued_id
          for e in tree.runs.load_events_sync(worker.id)),
      what="the disk-headroom withheld event")
  events = tree.runs.load_events_sync(worker.id)
  assert not tree.runs.stop_requested(events, queued_id)
  run = await tree.runs.get_run(worker.id, queued_id)
  assert run is not None and run.pid is None
  assert tree.runs.terminal_outcome(events, queued_id) is None
  await conftest._settle_parent(tree, manager, timeout=5.0, poll=0.02)


@pytest.mark.asyncio
async def test_reconcile_settles_a_queued_run_on_a_completed_node_once(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A completed node holding a queued run with no stop request: one startup
    pass records exactly one stop request (request id closed-task:<run id>),
    and a repeated pass records nothing more."""
  cfg, _session_blocks, tree = test_task_execution.build_env(tmp_path, monkeypatch)
  worker = await conftest.create_task(
      tree, parent=None, request_id="w", profile="worker", task=models.TaskSpec(goal="g"))
  await tree.runs.register_run_locked(models.RunRecord(id="run-done", session_id=worker.id, kind="work"))
  await tree.dispatch.finish_run(worker.id, "run-done", outcome="success", exit_code=0)
  assert tree.task_state(worker.id) == "completed"
  await tree.runs.register_run_locked(
      models.RunRecord(id="run-queued", session_id=worker.id, kind="work", backend="fake", model="fake-model"))

  await task_recovery.reconcile_task_tree(cfg, tree)

  def stops() -> list[dict]:
    events = tree.runs.load_events_sync(worker.id)
    return [e for e in events if e["type"] == ET.RUN_STOP_REQUESTED and e.get("run_id") == "run-queued"]

  assert len(stops()) == 1 and stops()[0]["request_id"] == "closed-task:run-queued"
  facts = len(tree.fact_history(worker.id))
  await task_recovery.reconcile_task_tree(cfg, tree)
  assert len(stops()) == 1
  assert len(tree.fact_history(worker.id)) == facts


def _count_landing_git(monkeypatch: pytest.MonkeyPatch) -> dict[str, int]:
  """Count the landing-proof git calls at src.infra.git's module attributes.

    task_completion and task_execution resolve git_verify_commit_landed through
    the src.infra.git module attribute, so the counter replaces the module
    attribute; git_verify_commit_landed itself resolves git_fetch through the
    same module global, so both counters see every call a replayed follow-up
    makes.
    """
  from src.infra import git as git_mod

  counts = {"git_fetch": 0, "git_verify_commit_landed": 0}
  real_fetch = git_mod.git_fetch
  real_verify = git_mod.git_verify_commit_landed

  async def counted_fetch(repo_path, remote, branch):
    counts["git_fetch"] += 1
    return await real_fetch(repo_path, remote, branch)

  async def counted_verify(repo_path, branch, commit):
    counts["git_verify_commit_landed"] += 1
    return await real_verify(repo_path, branch, commit)

  monkeypatch.setattr(git_mod, "git_fetch", counted_fetch)
  monkeypatch.setattr(git_mod, "git_verify_commit_landed", counted_verify)
  return counts


async def _reviewed_implement_task(tmp_path, monkeypatch):
  """One implement worker whose work and review Runs both finished success with
    its reviewed branch landed on origin's base — the durable state every server
    restart reconciles: every Run terminal, the review's follow-up owed to the
    replay. The worktree is already gone from disk, as a delivered task's is."""

  cfg, session_blocks, tree, manager, worker = await _manager_and_worker(
      tmp_path, monkeypatch, task_type=models.TaskType.IMPLEMENT)
  repo, _origin = conftest.init_repo_with_origin(tmp_path / "repo")
  # The reviewer's landing: the reviewed branch fast-forwards origin's base.
  conftest.run_git(repo, "checkout", "-q", "-b", "task/work")
  (repo / "marker.txt").write_text("implemented\n")
  conftest.run_git(repo, "add", "-A")
  conftest.run_git(repo, "commit", "-q", "-m", "implement marker")
  conftest.run_git(repo, "push", "-q", "origin", "task/work:main")
  conftest.install_scripted_backends(
      monkeypatch, [test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("review ok")])],
      conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)
  # The delivered report's parent turn: one fresh scripted double per build.
  monkeypatch.setattr(conftest.BUILD_BACKEND_PATCH_TARGET, test_task_execution.make_pm_build("report consumed"))
  work_run_id = "run-work"
  await tree.runs.register_run_locked(
      models.RunRecord(
          id=work_run_id,
          session_id=worker.id,
          kind="work",
          backend="fake",
          model="fake-model",
          repo_path=str(repo),
          base_branch="main",
          branch_name="task/work",
          worktree_path=str(tmp_path / "charliebot-home" / "worktrees" / "task-work-gone")))
  await tree.dispatch.finish_run(worker.id, work_run_id, outcome="success", exit_code=0)
  review_run_id = "review-done"
  await tree.runs.register_run_locked(
      models.RunRecord(
          id=review_run_id,
          session_id=worker.id,
          kind="review",
          review_of_run_id=work_run_id,
          backend="fake",
          model="fake-model"))
  await tree.dispatch.finish_run(worker.id, review_run_id, outcome="success", exit_code=0)
  return cfg, session_blocks, tree, manager, worker


@pytest.mark.asyncio
async def test_recovery_closed_task_skips_landing(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A closed task's replayed review follow-up re-proves nothing: repeated
    startup passes make zero landing git calls, append no fact on the worker or
    its parent, and leave the state completed."""

  cfg, _session_blocks, tree, manager, worker = await _reviewed_implement_task(tmp_path, monkeypatch)
  # The warm pass replays the follow-up on the still-open task: the landing
  # proof runs once, the automatic close lands, and the parent report's own
  # turn settles before the counted passes start.
  await task_recovery.reconcile_task_tree(cfg, tree)
  assert tree.task_state(worker.id) == "completed"
  await conftest._settle_parent(tree, manager, timeout=5.0, poll=0.02)

  counts = _count_landing_git(monkeypatch)
  worker_facts = len(tree.fact_history(worker.id))
  manager_facts = len(tree.fact_history(manager.id))
  for _round in range(2):
    await task_recovery.reconcile_task_tree(cfg, tree)
  assert counts["git_fetch"] == 0, counts
  assert counts["git_verify_commit_landed"] == 0, counts
  assert len(tree.fact_history(worker.id)) == worker_facts
  assert len(tree.fact_history(manager.id)) == manager_facts
  assert tree.task_state(worker.id) == "completed"


@pytest.mark.asyncio
async def test_recovery_reopened_task_reproves_landing(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A restored task derives "open" again, so its replayed review follow-up
    runs the full landing proof (the counter observes git_verify_commit_landed)."""

  cfg, _session_blocks, tree, manager, worker = await _reviewed_implement_task(tmp_path, monkeypatch)
  await task_recovery.reconcile_task_tree(cfg, tree)
  assert tree.task_state(worker.id) == "completed"
  await conftest._settle_parent(tree, manager, timeout=5.0, poll=0.02)
  await tree.completion.restore_task(
      worker.id, request_id="reopen-1", reason="recheck the delivery", caller=conftest.OPERATOR)
  assert tree.task_state(worker.id) == "open"

  counts = _count_landing_git(monkeypatch)
  await task_recovery.reconcile_task_tree(cfg, tree)
  assert counts["git_verify_commit_landed"] >= 1, counts


@pytest.mark.asyncio
async def test_recovery_open_task_landing_unchanged(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """An open task's replayed review follow-up proves the landing and closes:
    one pass closes the task and delivers the completed report to the parent,
    exactly as the closed-task skip left it."""

  cfg, _session_blocks, tree, manager, worker = await _reviewed_implement_task(tmp_path, monkeypatch)
  counts = _count_landing_git(monkeypatch)
  await task_recovery.reconcile_task_tree(cfg, tree)
  assert counts["git_verify_commit_landed"] >= 1, counts
  assert counts["git_fetch"] >= 1, counts
  assert tree.task_state(worker.id) == "completed"
  reports = [e for e in tree.fact_history(manager.id) if e.get("type") == ET.CHILD_REPORT]
  assert reports and reports[-1]["outcome"] == "completed"
  await conftest._settle_parent(tree, manager, timeout=5.0, poll=0.02)


@pytest.mark.asyncio
async def test_recovery_after_a_failed_review_picks_the_next_preference_backend(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A failed review is a used attempt: recovery registers the next review
    on the next pool backend (the existing policy, unchanged)."""

  cfg, _session_blocks, tree, _manager, worker = await _manager_and_worker(tmp_path, monkeypatch)
  # Two reviewer entries beyond the worker's own backend: a failed first
  # attempt must move to the second one.
  cfg.backends.options.extend(
      [
          conftest.backend_option(id="fake2", label="Fake2", type="cc-claude", model="fake2-model"),
          conftest.backend_option(id="fake3", label="Fake3", type="cc-claude", model="fake3-model"),
      ])
  cfg.backends.pool = ["fake2", "fake3"]
  repo, _origin = conftest.init_repo_with_origin(tmp_path / "repo")
  await tree.patch_task(
      worker.id,
      models.PatchSessionTaskRequest(
          task=models.TaskSpec(goal="do the work", repo_path=str(repo), task_type=models.TaskType.IMPLEMENT)),
      caller=conftest.OPERATOR)
  conftest.install_scripted_backends(
      monkeypatch, [test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("review ok")])],
      conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)
  run_id = "run-work"
  await tree.runs.register_run_locked(
      models.RunRecord(
          id=run_id,
          session_id=worker.id,
          kind="work",
          backend="fake",
          model="fake-model",
          repo_path=str(repo),
          base_branch="main",
          branch_name="task/work",
          worktree_path=str(repo)))
  await tree.dispatch.finish_run(worker.id, run_id, outcome="success", exit_code=0)
  # The first review attempt ran on the first pool backend and failed.
  failed_review = "review-failed"
  await tree.runs.register_run_locked(
      models.RunRecord(
          id=failed_review,
          session_id=worker.id,
          kind="review",
          review_of_run_id=run_id,
          backend="fake2",
          model="fake2-model"))
  await tree.dispatch.finish_run(worker.id, failed_review, outcome="failed", exit_code=1)

  await task_recovery.reconcile_task_tree(cfg, tree)
  reviews = [r for r in tree.runs.list_run_records_sync(worker.id) if r.kind == "review"]
  assert len(reviews) == 2, f"expected the retried review, got {[r.id for r in reviews]}"
  retried = next(r for r in reviews if r.id != failed_review)
  # The stable retry identity binds to the work run and its attempt number.
  assert retried.id == control_events.stable_run_id(worker.id, f"review:{run_id}:2")
  assert retried.backend == "fake3", f"expected the next pool backend, got {retried.backend}"
  _run, outcome = await test_task_execution.wait_for_terminal_run(tree, worker.id, retried.id)
  assert outcome == "success"
  # The retried review consumed the one scripted backend; nothing else launches.
  await task_recovery.reconcile_task_tree(cfg, tree)
  assert len([r for r in tree.runs.list_run_records_sync(worker.id) if r.kind == "review"]) == 2


async def _cold_read_child(tmp_path: pathlib.Path, tree, manager, *, page: pathlib.Path):
  """One cold-read worker child under the manager, its page already on disk."""
  page.write_text("<html><body><p>sitrep body</p></body></html>", encoding="utf-8")
  return await tree.create_task(
      request_id="cold-read",
      task_parent_id=manager.id,
      profile="worker",
      task=models.TaskSpec(
          goal=f"cold read of {page}",
          task_type=models.TaskType.COLD_READ,
          cold_read=models.ColdReadSpec(page_path=str(page), genre="sitrep", trigger="where are we?")),
      name=f"Cold read: {page.name}",
      backend="fake",
      caller=conftest.OPERATOR)


@pytest.mark.asyncio
async def test_recovery_replays_a_lost_cold_read_delivery(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A cold-read Run whose terminal fact landed but whose delivery a crash cut:
    the replay reports the parent with the report body and closes the child; a
    repeated pass lands nothing twice."""
  cfg, _session_blocks, tree, manager = await _takeoff_manager(tmp_path, monkeypatch)
  page = tmp_path / "page.html"
  worker = await _cold_read_child(tmp_path, tree, manager, page=page)
  run_id = "run-cold"
  await tree.runs.register_run_locked(
      models.RunRecord(id=run_id, session_id=worker.id, kind="work", backend="fake", model="fake-model"))
  await ndjson.append_ndjson(
      tree.runs.run_dir(worker.id, run_id) / runs.RUN_EVENTS_NAME, test_task_execution.result_event("(1) answered."))
  await tree.dispatch.finish_run(worker.id, run_id, outcome="success", exit_code=0)

  await task_recovery.reconcile_task_tree(cfg, tree)
  reports = conftest.child_reports(tree, manager.id)
  assert len(reports) == 1
  assert reports[-1]["outcome"] == "completed"
  assert reports[-1]["summary"] == "\n".join(
      [
          f"cold read of {page}",
          f"page sha256 {hashlib.sha256(page.read_bytes()).hexdigest()}",
          "backend fake",
          "(1) answered.",
      ])
  assert tree.task_state(worker.id) == "completed"
  await conftest._settle_parent(tree, manager, timeout=5.0, poll=0.02)
  # A repeated pass lands nothing twice.
  await task_recovery.reconcile_task_tree(cfg, tree)
  assert len(conftest.child_reports(tree, manager.id)) == 1
  assert len(tree.runs.list_run_records_sync(worker.id)) == 1
  assert tree.task_state(worker.id) == "completed"


@pytest.mark.asyncio
async def test_recovery_replays_a_lost_cold_read_failure_chain(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A failed cold-read Run whose chain a crash cut with one backend left: the
    replay records the attempt, starts the retry on the next pool
    backend, and the retry's answer delivers the report."""
  cfg, _session_blocks, tree, manager = await _takeoff_manager(tmp_path, monkeypatch)
  cfg.backends.options.append(conftest.backend_option(id="fake2", label="Fake2", type="codex", model="fake2-model"))
  cfg.backends.pool = ["fake2"]
  page = tmp_path / "page.html"
  worker = await _cold_read_child(tmp_path, tree, manager, page=page)
  failed_run = "run-cold-1"
  await tree.runs.register_run_locked(
      models.RunRecord(id=failed_run, session_id=worker.id, kind="work", backend="fake", model="fake-model"))
  await tree.dispatch.finish_run(worker.id, failed_run, outcome="failed", exit_code=1)
  test_task_execution.install_worker_launch_and_resume_backends(
      monkeypatch, [test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("(1) answered.")])])

  await task_recovery.reconcile_task_tree(cfg, tree)
  await test_task_execution.poll_until(
      lambda: bool(conftest.child_reports(tree, manager.id)), timeout=15.0, what="the cold-read child report")
  reports = conftest.child_reports(tree, manager.id)
  assert reports[-1]["outcome"] == "completed"
  assert reports[-1]["summary"] == "\n".join(
      [
          f"cold read of {page}",
          f"page sha256 {hashlib.sha256(page.read_bytes()).hexdigest()}",
          "attempt fake failed: run run-cold-1 failed without a reportable output",
          "backend fake2",
          "(1) answered.",
      ])
  assert tree.task_state(worker.id) == "completed"
  await conftest._settle_parent(tree, manager, timeout=5.0, poll=0.02)
  # A repeated pass lands nothing twice.
  await task_recovery.reconcile_task_tree(cfg, tree)
  assert len(conftest.child_reports(tree, manager.id)) == 1
  assert len(tree.runs.list_run_records_sync(worker.id)) == 2
  assert tree.task_state(worker.id) == "completed"


@pytest.mark.asyncio
async def test_recovery_replays_a_lost_cold_read_all_failed_report(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A failed cold-read Run whose chain a crash cut with no backend left: the
    replay reports the parent with the attempt line and the all-failed trailer,
    and the report's delivery archives the child; a repeated pass lands nothing
    twice."""
  cfg, _session_blocks, tree, manager = await _takeoff_manager(tmp_path, monkeypatch)
  page = tmp_path / "page.html"
  worker = await _cold_read_child(tmp_path, tree, manager, page=page)
  failed_run = "run-cold-1"
  await tree.runs.register_run_locked(
      models.RunRecord(id=failed_run, session_id=worker.id, kind="work", backend="fake", model="fake-model"))
  await tree.dispatch.finish_run(worker.id, failed_run, outcome="failed", exit_code=1)

  await task_recovery.reconcile_task_tree(cfg, tree)
  reports = conftest.child_reports(tree, manager.id)
  assert len(reports) == 1
  assert reports[-1]["outcome"] == "failed"
  assert reports[-1]["summary"] == "\n".join(
      [
          f"cold read of {page}",
          f"page sha256 {hashlib.sha256(page.read_bytes()).hexdigest()}",
          "attempt fake failed: run run-cold-1 failed without a reportable output",
          "every model failed (1 tried)",
      ])
  assert tree.task_state(worker.id) == "archived"
  closes = [e for e in tree.events.load_events(worker.id) if e["type"] == ET.TASK_CLOSED]
  assert len(closes) == 1
  assert closes[0]["outcome"] == "archived"
  assert closes[0]["actor"] == "system"
  assert closes[0]["request_id"] == f"unsuccessful:{failed_run}"
  await conftest._settle_parent(tree, manager, timeout=5.0, poll=0.02)
  # A repeated pass lands nothing twice.
  await task_recovery.reconcile_task_tree(cfg, tree)
  assert len(conftest.child_reports(tree, manager.id)) == 1
  assert len(tree.runs.list_run_records_sync(worker.id)) == 1
  assert len([e for e in tree.events.load_events(worker.id) if e["type"] == ET.TASK_CLOSED]) == 1
  assert tree.task_state(worker.id) == "archived"


@pytest.mark.asyncio
async def test_recovery_replays_a_lost_cold_read_interrupted_report(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """An interrupted cold-read Run reports at replay exactly as the live
    chain's fall-through does (never through the next-backend retry), and the
    report's delivery archives the child; a repeated pass lands nothing twice."""
  cfg, _session_blocks, tree, manager = await _takeoff_manager(tmp_path, monkeypatch)
  page = tmp_path / "page.html"
  worker = await _cold_read_child(tmp_path, tree, manager, page=page)
  run_id = "run-cold-1"
  await tree.runs.register_run_locked(
      models.RunRecord(id=run_id, session_id=worker.id, kind="work", backend="fake", model="fake-model"))
  await tree.dispatch.finish_run(worker.id, run_id, outcome="interrupted", exit_code=-1)

  await task_recovery.reconcile_task_tree(cfg, tree)
  reports = conftest.child_reports(tree, manager.id)
  assert [r["outcome"] for r in reports] == ["interrupted"]
  # No retry run: an interrupted cold read never takes the next-backend chain.
  assert len(tree.runs.list_run_records_sync(worker.id)) == 1
  assert tree.task_state(worker.id) == "archived"
  closes = [e for e in tree.events.load_events(worker.id) if e["type"] == ET.TASK_CLOSED]
  assert len(closes) == 1
  assert closes[0]["actor"] == "system"
  assert closes[0]["request_id"] == f"unsuccessful:{run_id}"
  assert closes[0]["summary"] == f"run {run_id} ended interrupted; archived by the server"
  await conftest._settle_parent(tree, manager, timeout=5.0, poll=0.02)
  # A repeated pass lands nothing twice.
  await task_recovery.reconcile_task_tree(cfg, tree)
  assert len(conftest.child_reports(tree, manager.id)) == 1
  assert len([e for e in tree.events.load_events(worker.id) if e["type"] == ET.TASK_CLOSED]) == 1


@pytest.mark.asyncio
async def test_recovery_replays_an_earlier_failed_run_without_archiving_a_node_a_later_run_finished(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The last-ended verdict: a node whose failed run A preceded a successful
    run B keeps A's replayed report (one, waking the parent once) but is never
    archived by it — run B ended last."""
  cfg, _session_blocks, tree, manager, worker = await _manager_and_worker(
      tmp_path, monkeypatch, task_type=models.TaskType.IMPLEMENT)
  monkeypatch.setattr(conftest.BUILD_BACKEND_PATCH_TARGET, test_task_execution.make_pm_build("manager turn"))
  await tree.runs.register_run_locked(
      models.RunRecord(id="run-a", session_id=worker.id, kind="work", backend="fake", model="fake-model"))
  await tree.dispatch.finish_run(worker.id, "run-a", outcome="failed", exit_code=1)
  await tree.runs.register_run_locked(
      models.RunRecord(id="run-b", session_id=worker.id, kind="work", backend="fake", model="fake-model"))
  await tree.dispatch.finish_run(worker.id, "run-b", outcome="success", exit_code=0)

  await task_recovery.reconcile_task_tree(cfg, tree)
  reports = conftest.child_reports(tree, manager.id)
  assert [r["outcome"] for r in reports] == ["failed"]
  assert [r["result_refs"] for r in reports] == [["run:run-a"]]
  # Run B ended last and awaits its review: the node stays open, unarchived.
  assert tree.task_state(worker.id) == "open"
  assert [e for e in tree.events.load_events(worker.id) if e["type"] == ET.TASK_CLOSED] == []
  await conftest._settle_parent(tree, manager, timeout=5.0, poll=0.02)


@pytest.mark.asyncio
async def test_recovery_archives_an_open_worker_whose_failure_report_the_parent_already_holds(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The pre-fix leftover: an open worker whose last run failed and whose
    report already sits in the parent's log. The startup pass re-delivers
    nothing (the stable report id dedups, no wake) and archives the worker; a
    second pass writes no second fact."""
  cfg, _session_blocks, tree, manager, worker = await _manager_and_worker(tmp_path, monkeypatch)
  monkeypatch.setattr(conftest.BUILD_BACKEND_PATCH_TARGET, test_task_execution.make_pm_build("manager turn"))
  run_id = "run-failed"
  await tree.runs.register_run_locked(
      models.RunRecord(id=run_id, session_id=worker.id, kind="work", backend="fake", model="fake-model"))
  await tree.dispatch.finish_run(worker.id, run_id, outcome="failed", exit_code=1)
  # The pre-fix live path's delivery: the one report in the parent's log,
  # consumed by the parent's own turn, the node still open.
  events = tree.runs.load_events_sync(worker.id)
  source = next(e for e in reversed(events) if e["type"] == ET.RUN_FINISHED and e["run_id"] == run_id)
  _report, created = await tree.dispatch.deliver_child_report(
      worker.id,
      source_event=source,
      outcome="failed",
      summary=f"run {run_id} failed",
      result_refs=[f"run:{run_id}"],
      recipient=manager.id)
  assert created
  assert tree.task_state(worker.id) == "open"

  def report_turns() -> list:
    return [r for r in tree.runs.list_run_records_sync(manager.id) if r.kind == "manager_turn"]

  await test_task_execution.poll_until(
      lambda: len(report_turns()) == 2, timeout=15.0, what="the setup delivery's parent turn")
  await conftest._settle_parent(tree, manager, timeout=5.0, poll=0.02)

  await task_recovery.reconcile_task_tree(cfg, tree)
  # No second report and no second wake: the manager holds exactly the one
  # delivery and its takeoff + report turns.
  assert len(conftest.child_reports(tree, manager.id)) == 1
  assert len(report_turns()) == 2
  assert tree.task_state(worker.id) == "archived"
  closes = [e for e in tree.events.load_events(worker.id) if e["type"] == ET.TASK_CLOSED]
  assert len(closes) == 1
  assert closes[0]["outcome"] == "archived"
  assert closes[0]["actor"] == "system"
  assert closes[0]["request_id"] == f"unsuccessful:{run_id}"

  # A second startup writes no second fact.
  await task_recovery.reconcile_task_tree(cfg, tree)
  assert len(conftest.child_reports(tree, manager.id)) == 1
  assert len(report_turns()) == 2
  assert len([e for e in tree.events.load_events(worker.id) if e["type"] == ET.TASK_CLOSED]) == 1


@pytest.mark.asyncio
async def test_recovery_cold_read_failure_with_a_registered_retry_never_double_retries(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A failed cold-read Run whose retry a crash left registered but unlaunched:
    the replay launches the queued retry (step 4) and never re-drives the failed
    attempt's chain — one retry Run, one report."""
  cfg, _session_blocks, tree, manager = await _takeoff_manager(tmp_path, monkeypatch)
  cfg.backends.options.append(conftest.backend_option(id="fake2", label="Fake2", type="codex", model="fake2-model"))
  cfg.backends.pool = ["fake2"]
  page = tmp_path / "page.html"
  worker = await _cold_read_child(tmp_path, tree, manager, page=page)
  failed_run = "run-cold-1"
  await tree.runs.register_run_locked(
      models.RunRecord(id=failed_run, session_id=worker.id, kind="work", backend="fake", model="fake-model"))
  await tree.dispatch.finish_run(worker.id, failed_run, outcome="failed", exit_code=1)
  retry_run = "run-cold-2"
  await tree.runs.register_run_locked(
      models.RunRecord(id=retry_run, session_id=worker.id, kind="work", backend="fake2", model="fake2-model"))
  test_task_execution.install_worker_launch_and_resume_backends(
      monkeypatch, [test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("(1) answered.")])])

  await task_recovery.reconcile_task_tree(cfg, tree)
  await test_task_execution.poll_until(
      lambda: bool(conftest.child_reports(tree, manager.id)), timeout=15.0, what="the cold-read child report")
  reports = conftest.child_reports(tree, manager.id)
  assert reports[-1]["outcome"] == "completed"
  assert "backend fake2" in reports[-1]["summary"].splitlines()
  assert tree.task_state(worker.id) == "completed"
  work_runs = [r for r in tree.runs.list_run_records_sync(worker.id) if r.kind == "work"]
  assert [r.id for r in work_runs] == [failed_run, retry_run]


@pytest.mark.asyncio
async def test_reconcile_replays_an_already_delivered_blocked_report_without_duplication(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The startup pass never wakes a parent for an already delivered report.

    The delivery-replay path: an implement child whose landing check keeps
    failing (a rebase rewrote the base) re-derives the same stable blocked
    report id on every restart, and the pass replays every terminal review
    run. The reconcile of the same state — two successful review runs, the
    report already in the manager's log — must append nothing.
    """

  cfg, session_blocks, tree = test_task_execution.build_env(
      tmp_path, monkeypatch, backend_ids=["fake", "fake2"], pool=["fake", "fake2"])
  repo, _origin = conftest.init_repo_with_origin(tmp_path)
  tree.dispatch.executor = test_task_execution._adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  manager = await conftest.create_root_session(
      session_blocks, models.CreateSessionRequest(name="Manager"), backend="fake")
  worker = await tree.create_task(
      request_id="w",
      task_parent_id=manager.id,
      profile="worker",
      task=models.TaskSpec(
          goal="add a marker", repo_path=str(repo), base_branch="main", task_type=models.TaskType.IMPLEMENT),
      name="W",
      backend=None,
      caller="operator")

  # The reviewer never pushes, so the landing check fails on every pass.
  conftest.install_scripted_backends(
      monkeypatch, [
          test_task_execution.SpawningScriptedBackend(
              [test_task_execution.result_event("implemented")],
              pre_run=functools.partial(test_task_execution.implement_marker_commit, tree, worker.id)),
          test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("review ok")])
      ], conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)
  # The blocked report below wakes the manager. Script that turn too: left
  # unstubbed it builds a real codex, whose failure timing decides whether
  # the turn is terminal when the startup pass runs (instant crash on CI,
  # a 20s+ 401-retry flail on a checkout with credentials), and that race
  # alone flips the followup count the pass reports.
  monkeypatch.setattr(conftest.BUILD_BACKEND_PATCH_TARGET, test_task_execution.make_pm_build("manager turn"))

  await tree.dispatch.admit_input(worker.id, event_type=ET.USER, content="Start the work.", actor="user")
  decision = await tree.dispatch.dispatch_pending(worker.id)
  _work, outcome = await test_task_execution.wait_for_terminal_run(tree, worker.id, decision["run_id"])
  assert outcome == "success"

  deadline = asyncio.get_event_loop().time() + 15
  while asyncio.get_event_loop().time() < deadline:
    reports = [e for e in tree.events.load_events(manager.id) if e.get("type") == ET.CHILD_REPORT]
    if reports:
      break
    await asyncio.sleep(0.05)
  else:
    pytest.fail("the review chain never reported to the manager")
  assert [r["outcome"] for r in reports] == ["blocked"]
  # The blocked report's delivery archived the stopped worker; the archive
  # rides the review run's delivery chain, so it can land after the report.
  await test_task_execution.poll_until(
      lambda: tree.task_state(worker.id) == "archived", timeout=15.0, what="the blocked worker's archive")

  # The blocked report wakes the manager, and its turn runs in flight beside
  # the steps below. The startup pass below runs boot-style — no is_driven
  # filter, its premise being that nothing in this process is in flight — so
  # a manager_turn that has already landed its terminal fact adds its
  # owner-close recheck to the followup count on a loaded runner while a fast
  # checkout still sees it unlanded. Settle the turn first so the pass reads
  # the same terminal-run set everywhere; replaying it is idempotent (its
  # request-id replay dedups).
  await test_task_execution.poll_until(
      lambda: any(r.kind == "manager_turn" for r in tree.runs.list_run_records_sync(manager.id)),
      timeout=15.0,
      what="the blocked report's manager turn")
  turn = next(r for r in tree.runs.list_run_records_sync(manager.id) if r.kind == "manager_turn")
  await test_task_execution.wait_for_terminal_run(tree, manager.id, turn.id)

  # A second successful review run of the same work run replays alongside the first.
  work_run = next(r for r in tree.runs.list_run_records_sync(worker.id) if r.kind == "work")
  await tree.runs.register_run_locked(
      models.RunRecord(
          id="review-replay-2",
          session_id=worker.id,
          kind="review",
          review_of_run_id=work_run.id,
          backend="fake",
          model="fake-model",
          repo_path=work_run.repo_path,
          base_branch=work_run.base_branch,
          branch_name=work_run.branch_name))
  await tree.runs.record_finish(worker.id, "review-replay-2", "success")

  counters = await task_recovery.reconcile_task_tree(cfg, tree)
  # The work run plus both review runs replayed, plus the settled manager
  # turn's owner-close recheck.
  assert counters["followups"] == 4
  reports = [e for e in tree.events.load_events(manager.id) if e.get("type") == ET.CHILD_REPORT]
  assert len(reports) == 1


# ---------------------------------------------------------------------------
# End-landing retry: the node reconcile pass re-run on out-of-space endings
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_retry_and_boot_reconcile_repair_half_written_end_metadata(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """(f) A run_finished fact whose metadata write failed out of space: the
    retry fills ended_at/exit_code from the raw log once space returns, and a
    boot reconcile fills them too — from the drain rule's values, once."""
  monkeypatch.setattr(task_execution_module, "RUN_END_LANDING_RETRY_INTERVAL_SECONDS", 0.05)
  cfg, session_blocks, tree, manager, worker = await _manager_and_worker(tmp_path, monkeypatch)
  adapter = test_task_execution._adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  tree.dispatch.executor = adapter
  test_task_execution.install_worker_launch_and_resume_backends(
      monkeypatch, [test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("work done")])])
  monkeypatch.setattr(conftest.BUILD_BACKEND_PATCH_TARGET, test_task_execution.make_pm_build("report consumed"))

  await tree.dispatch.admit_input(worker.id, event_type=ET.USER, content="Start the task.", actor="user")
  decision = await tree.dispatch.dispatch_pending(worker.id)
  run_id = decision["run_id"]
  run_dir = tree.runs.run_dir(worker.id, run_id)
  staged: list = []

  def _stage_raw_log() -> None:
    staged.append(test_task_execution.write_raw_result(run_dir, "drained work result"))

  # The finish's metadata write (the one carrying ended_at) fails out of
  # space after the fact landed; the fault stages the drain's raw log.
  test_task_execution.inject_run_record_write_fault(monkeypatch, only_finished=True, times=1, on_raise=_stage_raw_log)
  run, _outcome = await test_task_execution.wait_for_terminal_run(tree, worker.id, run_id)
  assert len(staged) == 1
  raw_mtime = staged[0][1]
  # (The half-written state itself is transient — the retry repairs it within
  # the first round — and is staged explicitly for the boot half below.)
  await conftest._settle_parent(tree, manager, timeout=5.0, poll=0.02)
  run = await tree.runs.get_run(worker.id, run_id)
  assert run is not None
  assert run.ended_at is not None and abs(run.ended_at - raw_mtime) < datetime.timedelta(seconds=1)
  assert run.exit_code == 0  # the raw log's successful result event
  finished = [e for e in tree.fact_history(worker.id) if e.get("type") == ET.RUN_FINISHED and e.get("run_id") == run_id]
  assert len(finished) == 1  # the retry never wrote a second end record

  # A boot reconcile repairs the same shape too: a staged fact with empty
  # metadata and a past-stamped raw log.
  cfg2, session_blocks2, tree2, _manager2, worker2 = await _manager_and_worker(tmp_path, monkeypatch)
  tree2.dispatch.executor = test_task_execution._adapter_with_silent_broadcast(
      cfg2, session_blocks2, tree2, monkeypatch)
  test_task_execution.install_worker_launch_and_resume_backends(
      monkeypatch, [test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("staged work")])])
  boot_run_id = "run-half-written"
  await tree2.runs.register_run_locked(
      models.RunRecord(id=boot_run_id, session_id=worker2.id, kind="work", backend="fake", model="fake-model"))
  await tree2.dispatch.finish_run(worker2.id, boot_run_id, outcome="success", exit_code=0)
  meta_path = tree2.runs.metadata_path(worker2.id, boot_run_id)
  record = json.loads(meta_path.read_text(encoding="utf-8"))
  record["ended_at"] = None
  record["exit_code"] = None
  meta_path.write_text(json.dumps(record), encoding="utf-8")
  _raw, boot_mtime = test_task_execution.write_raw_result(
      tree2.runs.run_dir(worker2.id, boot_run_id), "boot result", age_seconds=1800)
  await task_recovery.reconcile_task_tree(cfg2, tree2, tree2.dispatch.executor)
  boot_run = await tree2.runs.get_run(worker2.id, boot_run_id)
  assert boot_run is not None
  assert boot_run.ended_at is not None and abs(boot_run.ended_at - boot_mtime) < datetime.timedelta(seconds=1)
  assert boot_run.exit_code == 0


@pytest.mark.asyncio
async def test_boot_node_out_of_space_hands_node_to_retry(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """(g) Boot reconcile's node pass failing out of space hands the node to
    the retry entry; the retry's own round delivers the parent report."""
  monkeypatch.setattr(task_execution_module, "RUN_END_LANDING_RETRY_INTERVAL_SECONDS", 0.05)
  cfg, session_blocks, tree, manager, worker = await _manager_and_worker(tmp_path, monkeypatch)
  adapter = test_task_execution._adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  tree.dispatch.executor = adapter
  monkeypatch.setattr(conftest.BUILD_BACKEND_PATCH_TARGET, test_task_execution.make_pm_build("report consumed"))
  await tree.runs.register_run_locked(
      models.RunRecord(id="run-failed", session_id=worker.id, kind="work", backend="fake", model="fake-model"))
  await tree.dispatch.finish_run(worker.id, "run-failed", outcome="failed", exit_code=-1)
  assert conftest.child_reports(tree, manager.id) == []

  # The boot pass's delivery write fails out of space: the node pass raises,
  # the boot hands the node to the retry, and the retry's round delivers.
  test_task_execution.inject_chat_append_fault(monkeypatch, event_type=ET.CHILD_REPORT, session_ids={manager.id})
  await task_recovery.reconcile_task_tree(cfg, tree, adapter)
  await test_task_execution.poll_until(
      lambda: len(conftest.child_reports(tree, manager.id)) == 1,
      what="the retry's parent report delivery",
      timeout=10.0)
  reports = conftest.child_reports(tree, manager.id)
  assert [r.get("outcome") for r in reports] == ["failed"]
  await test_task_execution.poll_until(
      lambda: worker.id not in adapter._landing_retries, what="the clean round ending the retry task")
  assert len(conftest.child_reports(tree, manager.id)) == 1  # exactly one report


@pytest.mark.asyncio
async def test_retry_round_skips_runs_this_process_already_drives(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """(h) A retry's node reconcile pass never makes a second follower for a
    run whose execute task is in flight, a run a previous round already
    follows, or a manager-turn follow queued in the master queue; replayed
    delivery starts no second review process."""
  monkeypatch.setattr(task_execution_module, "RUN_END_LANDING_RETRY_INTERVAL_SECONDS", 0.05)
  cfg, session_blocks, tree, manager, worker = await _manager_and_worker(
      tmp_path, monkeypatch, task_type=models.TaskType.IMPLEMENT)
  adapter = test_task_execution._adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  tree.dispatch.executor = adapter
  builds = test_task_execution.install_worker_launch_and_resume_backends(
      monkeypatch, [test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("late")])])
  monkeypatch.setattr(conftest.BUILD_BACKEND_PATCH_TARGET, test_task_execution.make_pm_build("report consumed"))

  # A run whose execute task is in flight, and a worker run a previous round
  # already follows: both stay this process's property.
  for run_key, pid in (("run-inflight", 424811), ("run-followed", 424812)):
    await tree.runs.register_run_locked(
        models.RunRecord(id=run_key, session_id=worker.id, kind="work", backend="fake", model="fake-model"))
    await tree.runs.record_launch(worker.id, run_key, pid=pid, pid_start="1-424000")
  adapter._launch_inflight.add((worker.id, "run-inflight"))
  adapter._resume_follows.add((worker.id, "run-followed"))

  # A delivered (terminal) work run whose review is registered but
  # unfinished: the replayed delivery re-enters _maybe_spawn_review, which
  # must return the existing review, never a second review process.
  await tree.runs.register_run_locked(
      models.RunRecord(id="run-work", session_id=worker.id, kind="work", backend="fake", model="fake-model"))
  await tree.dispatch.finish_run(worker.id, "run-work", outcome="success", exit_code=0)
  await tree.runs.register_run_locked(
      models.RunRecord(
          id="review-pending",
          session_id=worker.id,
          kind="review",
          review_of_run_id="run-work",
          backend="fake",
          model="fake-model"))

  # A manager-turn follow queued in the master queue: its future stays
  # pending until released, and the pair stays registered the whole time.
  await tree.runs.register_run_locked(
      models.RunRecord(id="run-mturn", session_id=manager.id, kind="manager_turn", backend="fake", model="fake-model"))
  await tree.runs.record_launch(manager.id, "run-mturn", pid=424813, pid_start="1-424000")
  released = asyncio.Event()
  resume_calls: list = []

  async def blocked_resume_cc(item):
    resume_calls.append(item)
    await asyncio.wait_for(released.wait(), timeout=5)
    return (None, 0, None, {})

  conftest.patch_resume_seams(monkeypatch, resume_cc=blocked_resume_cc)
  async with conftest.fresh_master_state(manager.id):
    await adapter.resume_run(manager.id, "run-mturn", is_alive=lambda: False)
    assert (manager.id, "run-mturn") in adapter._resume_follows

    driven = lambda run: adapter._drives_run(worker.id, run)  # noqa: E731
    counters = {"nodes": 0, "resumed": 0, "drained": 0, "followups": 0}
    await task_execution_module._reconcile_node(worker.id, tree, adapter, counters, cfg, is_driven=driven)
    # No second follower, no second review process: the skipped runs stay
    # non-terminal and the builds queue is untouched.
    assert builds == []
    for run_key in ("run-inflight", "run-followed"):
      run = await tree.runs.get_run(worker.id, run_key)
      assert run is not None and run.pid is not None
      assert tree.runs.terminal_outcome(tree.runs.load_events_sync(worker.id), run_key) is None
    reviews = [r for r in tree.runs.list_run_records_sync(worker.id) if r.kind == "review"]
    assert [r.id for r in reviews] == ["review-pending"]

    # The manager turn's round skips the queued follow too: still exactly
    # one resume call (the test's own), and the pair stays registered.
    await task_execution_module._reconcile_node(
        manager.id,
        tree,
        adapter, {
            "nodes": 0,
            "resumed": 0,
            "drained": 0,
            "followups": 0
        },
        cfg,
        is_driven=lambda run: adapter._drives_run(manager.id, run))
    assert len(resume_calls) == 1
    assert (manager.id, "run-mturn") in adapter._resume_follows

    # Releasing the future resolves the follow: the pair leaves the
    # registry only then, and the run lands exactly once.
    released.set()
    await test_task_execution.poll_until(
        lambda: (manager.id, "run-mturn") not in adapter._resume_follows, what="the manager-turn follow pair release")
    await test_task_execution.wait_for_terminal_run(tree, manager.id, "run-mturn")
    assert len(resume_calls) == 1

    # Without the driver the retry follows the worker run itself: the
    # registry pair was the only thing holding the round back.
    adapter._launch_inflight.discard((worker.id, "run-inflight"))
    await task_execution_module._reconcile_node(
        worker.id, tree, adapter, {
            "nodes": 0,
            "resumed": 0,
            "drained": 0,
            "followups": 0
        }, cfg)
    await test_task_execution.wait_for_terminal_run(tree, worker.id, "run-inflight")
