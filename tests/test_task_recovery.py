"""Startup-recovery tests: the v2 reconciliation pass converges every node.

Simulates restarts at the reservation, launch-identity, stop-request, terminal
append, review-creation, and parent-delivery boundaries, then runs the actual
startup pass (src.core.task_recovery.reconcile_task_tree) and asserts exact
input ownership, no duplicate processes/side effects/reports, repaired missing
follow-ups, and idempotence under a repeated pass. A second synthetic
instance's data and owned processes stay untouched.
"""

from __future__ import annotations

import asyncio
from functools import partial
from pathlib import Path

import pytest
from conftest import (
    BUILD_BACKEND_PATCH_TARGET,
    MASTER_TRIGGER_TRIGGER_MASTER_PATCH_TARGET,
    OPERATOR,
    WORKER_BUILD_BACKEND_PATCH_TARGET,
    _settle_parent,
    patch_instructions_content,
)

from src.core import event_types as ET
from src.core.models import CreateSessionRequest, RunRecord, TaskSpec, TaskType
from tests.test_parent_wake import drain_legacy_wakes
from tests.test_task_completion import wake_probe
from tests.test_task_execution import (
    SpawningScriptedBackend,
    _adapter_with_silent_broadcast,
    build_env,
    implement_marker_commit,
    init_repo_with_origin,
    install_backends,
    make_pm_build,
    result_event,
    wait_for_terminal_run,
)


class _IdentityTranslator:
    """A translate-only double: the scripted raw log already carries standard
    event dicts, so resume's stream translator passes them through."""

    _POST_RESULT_TIMEOUT = 5.0

    def translate_event(self, event: dict) -> list[dict]:
        # The raw bytes already carry standard event dicts; the stream
        # translator's contract is a list of projected events.
        return [event]


def install_resume_ready_backends(monkeypatch: pytest.MonkeyPatch, backends: list) -> list:
    """Serve launcher builds from *backends*; translate-only builds (resume)
    get the identity translator instead of consuming a scripted double."""
    builds: list = []
    queue = list(backends)

    def fake_build(option, cfg, **kwargs):
        if kwargs.get("on_spawn") is None:
            return _IdentityTranslator()
        backend = queue.pop(0)
        on_spawn = kwargs.get("on_spawn")
        if on_spawn is not None:
            backend.set_on_spawn(on_spawn)
        builds.append({"option": option, "backend": backend})
        return backend

    monkeypatch.setattr(WORKER_BUILD_BACKEND_PATCH_TARGET, fake_build)
    return builds


async def _takeoff_manager(tmp_path, monkeypatch):
    """One manager with its take-off turn already consumed (scripted backend)."""
    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
    tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
    patch_instructions_content(monkeypatch)
    install_backends(monkeypatch, [SpawningScriptedBackend([result_event("taken off")])],
                     BUILD_BACKEND_PATCH_TARGET)
    manager = await tree.create_task(
        request_id="root", task_parent_id=None, profile="manager",
        task=TaskSpec(goal="pm"), name="PM", backend=None, caller="operator")
    await tree.dispatch.admit_input(
        manager.id, event_type=ET.USER, content="Take off. Do the work.", actor="user")
    decision = await tree.dispatch.dispatch_pending(manager.id)
    assert decision["launch"] is True
    await wait_for_terminal_run(tree, manager.id, decision["run_id"])
    return cfg, session_mgr, tree, manager


async def _manager_and_worker(tmp_path, monkeypatch, task_type=None, repo=None):
    cfg, session_mgr, tree, manager = await _takeoff_manager(tmp_path, monkeypatch)
    child_task = TaskSpec(goal="do the work", repo_path=repo, task_type=task_type)
    worker = await tree.create_task(
        request_id="child", task_parent_id=manager.id, profile="worker",
        task=child_task, name="W", backend=None, caller="operator")
    return cfg, session_mgr, tree, manager, worker


@pytest.mark.asyncio
async def test_restart_before_launch_requeues_through_the_same_launch_checks(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """A run registered (reservation) but never launched: recovery dispatches it
    through the executor — one process, the exact registered provenance."""
    from src.core.task_recovery import reconcile_task_tree
    cfg, _session_mgr, tree, _manager, worker = await _manager_and_worker(tmp_path, monkeypatch)
    backend = SpawningScriptedBackend([result_event("recovered work")])
    builds = install_resume_ready_backends(monkeypatch, [backend])
    patch_instructions_content(monkeypatch)
    admitted = await tree.dispatch.admit_input(
        worker.id, event_type=ET.USER, content="Start the task.", actor="user")
    # Simulate the crash window: the reservation exists (batch claimed) but the
    # launch never happened — no pid, no terminal fact.
    run_id = "run-queued"
    await tree.runs.register_run(
        RunRecord(id=run_id, session_id=worker.id, kind="work",
                  backend="fake", model="fake-model"))
    await tree.dispatch.claim_input_batch_locked(worker.id, run_id)

    await reconcile_task_tree(cfg, tree)
    run, outcome = await wait_for_terminal_run(tree, worker.id, run_id)
    assert outcome == "success"
    assert run.pid == 424001
    assert run.input_event_ids == [str(admitted["id"])]
    assert len(builds) == 1
    # A repeated pass launches nothing new and duplicates nothing.
    await reconcile_task_tree(cfg, tree)
    assert len(tree.runs.list_run_records_sync(worker.id)) == 1
    assert len(builds) == 1


@pytest.mark.asyncio
async def test_restart_after_launch_reattaches_live_process(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """A recorded live (pid, pid_start) process is followed, never relaunched."""
    from src.core.task_recovery import reconcile_task_tree
    cfg, _session_mgr, tree, _manager, worker = await _manager_and_worker(tmp_path, monkeypatch)
    gate = asyncio.Event()
    backend = SpawningScriptedBackend([result_event("late result")], gate=gate.wait)
    builds = install_resume_ready_backends(monkeypatch, [backend])
    patch_instructions_content(monkeypatch)
    run_id = "run-live"
    await tree.runs.register_run(
        RunRecord(id=run_id, session_id=worker.id, kind="work",
                  backend="fake", model="fake-model"))
    # The crash window: launched and recorded, no terminal fact.
    await tree.runs.record_launch(worker.id, run_id, pid=424777, pid_start="1-424000")
    # The backend double's fake pid does not exist on this host: to exercise the
    # FOLLOW path, stub the liveness judgment — alive while the process works,
    # ended once the gate releases (the true→false transition the follower must
    # observe instead of a captured boolean).
    import src.core.runs as runs_mod
    monkeypatch.setattr(runs_mod, "is_run_alive", lambda *a, **k: not gate.is_set())

    async def _release_soon():
        await asyncio.sleep(0.3)
        gate.set()

    asyncio.get_event_loop().create_task(_release_soon())
    await reconcile_task_tree(cfg, tree)
    _run, outcome = await wait_for_terminal_run(tree, worker.id, run_id)
    # The follow observed the recorded process ENDING (true→false) and the run
    # converged to its durable result instead of staying falsely running. The
    # scripted double never wrote a raw stream (no launcher drove it), so the
    # honest outcome is failed — never a relaunch, never a fabricated success.
    assert outcome in ("failed", "interrupted")
    assert len(builds) == 0
    assert len(tree.runs.list_run_records_sync(worker.id)) == 1


@pytest.mark.asyncio
async def test_stop_request_wins_over_launch_and_recovery(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    from src.core.task_recovery import reconcile_task_tree
    cfg, _session_mgr, tree, _manager, worker = await _manager_and_worker(tmp_path, monkeypatch)
    builds = install_backends(monkeypatch, [SpawningScriptedBackend([result_event("x")])],
                              WORKER_BUILD_BACKEND_PATCH_TARGET)
    patch_instructions_content(monkeypatch)
    run_id = "run-stopped"
    await tree.runs.register_run(
        RunRecord(id=run_id, session_id=worker.id, kind="work",
                  backend="fake", model="fake-model"))
    await tree.dispatch.claim_input_batch_locked(worker.id, run_id)
    await tree.runs.request_stop(worker.id, run_id, "stop-1")
    await reconcile_task_tree(cfg, tree)
    # Stop precedence: the queued run never launches and never claims input;
    # the durable stop request stands and no side effect happened.
    events = tree.runs.load_events_sync(worker.id)
    assert tree.runs.stop_requested(events, run_id)
    assert builds == []
    stopped_run = await tree.runs.get_run(worker.id, run_id)
    assert stopped_run is not None and stopped_run.pid is None
    await reconcile_task_tree(cfg, tree)
    assert builds == []


@pytest.mark.asyncio
async def test_recovery_never_rereviews_a_successfully_reviewed_work_run(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """A work run whose review already succeeded never gets another review:
    restart recovery any number of times registers none (the chain ends at
    the first successful review), while the review's own follow-up replay
    (landing recheck) stays idempotent."""
    from src.core.models import PatchSessionTaskRequest
    from src.core.task_recovery import reconcile_task_tree
    from tests.test_task_execution import init_repo_with_origin
    cfg, _session_mgr, tree, _manager, worker = await _manager_and_worker(tmp_path, monkeypatch)
    repo, _origin = init_repo_with_origin(tmp_path / "repo")
    await tree.patch_task(
        worker.id,
        PatchSessionTaskRequest(
            task=TaskSpec(goal="do the work", repo_path=str(repo), task_type=TaskType.IMPLEMENT)),
        caller=OPERATOR)
    install_backends(
        monkeypatch, [SpawningScriptedBackend([result_event("review ok")])],
        WORKER_BUILD_BACKEND_PATCH_TARGET)
    patch_instructions_content(monkeypatch)
    run_id = "run-reviewed"
    await tree.runs.register_run(
        RunRecord(id=run_id, session_id=worker.id, kind="work",
                  backend="fake", model="fake-model",
                  repo_path=str(repo), base_branch="main",
                  branch_name="task/work", worktree_path=str(repo)))
    await tree.dispatch.finish_run(worker.id, run_id, outcome="success", exit_code=0)
    # The review ran to its own successful terminal fact (the state every
    # server restart used to re-enter with a fresh reviewer backend).
    review_id = "review-done"
    await tree.runs.register_run(
        RunRecord(id=review_id, session_id=worker.id, kind="review",
                  review_of_run_id=run_id, backend="fake", model="fake-model"))
    await tree.dispatch.finish_run(worker.id, review_id, outcome="success", exit_code=0)

    for _round in range(2):
        await reconcile_task_tree(cfg, tree)
        reviews = [r for r in tree.runs.list_run_records_sync(worker.id) if r.kind == "review"]
        assert len(reviews) == 1, f"recovery registered a new review: {[r.id for r in reviews]}"
        assert reviews[0].id == review_id


def _count_landing_git(monkeypatch: pytest.MonkeyPatch) -> dict[str, int]:
    """Count the landing-proof git calls at src.core.git's module attributes.

    task_execution imports git_verify_commit_landed inside _landing_for_work,
    so the module-attribute patch reaches it; git_verify_commit_landed itself
    resolves git_fetch through the same module global, so both counters see
    every call a replayed follow-up makes.
    """
    from src.core import git as git_mod

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
    from tests.test_task_execution import git as repo_git

    cfg, session_mgr, tree, manager, worker = await _manager_and_worker(
        tmp_path, monkeypatch, task_type=TaskType.IMPLEMENT)
    repo, _origin = init_repo_with_origin(tmp_path / "repo")
    # The reviewer's landing: the reviewed branch fast-forwards origin's base.
    repo_git(repo, "checkout", "-q", "-b", "task/work")
    (repo / "marker.txt").write_text("implemented\n")
    repo_git(repo, "add", "-A")
    repo_git(repo, "commit", "-q", "-m", "implement marker")
    repo_git(repo, "push", "-q", "origin", "task/work:main")
    install_backends(
        monkeypatch, [SpawningScriptedBackend([result_event("review ok")])],
        WORKER_BUILD_BACKEND_PATCH_TARGET)
    # The delivered report's parent turn: one fresh scripted double per build.
    monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, make_pm_build("report consumed"))
    patch_instructions_content(monkeypatch)
    work_run_id = "run-work"
    await tree.runs.register_run(
        RunRecord(id=work_run_id, session_id=worker.id, kind="work",
                  backend="fake", model="fake-model",
                  repo_path=str(repo), base_branch="main",
                  branch_name="task/work",
                  worktree_path=str(tmp_path / "charliebot-home" / "worktrees" / "task-work-gone")))
    await tree.dispatch.finish_run(worker.id, work_run_id, outcome="success", exit_code=0)
    review_run_id = "review-done"
    await tree.runs.register_run(
        RunRecord(id=review_run_id, session_id=worker.id, kind="review",
                  review_of_run_id=work_run_id, backend="fake", model="fake-model"))
    await tree.dispatch.finish_run(worker.id, review_run_id, outcome="success", exit_code=0)
    return cfg, session_mgr, tree, manager, worker


@pytest.mark.asyncio
async def test_recovery_closed_task_skips_landing(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """A closed task's replayed review follow-up re-proves nothing: repeated
    startup passes make zero landing git calls, append no fact on the worker or
    its parent, and leave the state completed."""
    from src.core.task_recovery import reconcile_task_tree

    cfg, _session_mgr, tree, manager, worker = await _reviewed_implement_task(tmp_path, monkeypatch)
    # The warm pass replays the follow-up on the still-open task: the landing
    # proof runs once, the automatic close lands, and the parent report's own
    # turn settles before the counted passes start.
    await reconcile_task_tree(cfg, tree)
    assert tree.task_state(worker.id) == "completed"
    await _settle_parent(tree, manager, timeout=5.0, poll=0.02)

    counts = _count_landing_git(monkeypatch)
    worker_facts = len(tree.fact_history(worker.id))
    manager_facts = len(tree.fact_history(manager.id))
    for _round in range(2):
        await reconcile_task_tree(cfg, tree)
    assert counts["git_fetch"] == 0, counts
    assert counts["git_verify_commit_landed"] == 0, counts
    assert len(tree.fact_history(worker.id)) == worker_facts
    assert len(tree.fact_history(manager.id)) == manager_facts
    assert tree.task_state(worker.id) == "completed"


@pytest.mark.asyncio
async def test_recovery_reopened_task_reproves_landing(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """A reopened task derives "open" again, so its replayed review follow-up
    runs the full landing proof (the counter observes git_verify_commit_landed)."""
    from src.core.task_recovery import reconcile_task_tree

    cfg, _session_mgr, tree, manager, worker = await _reviewed_implement_task(tmp_path, monkeypatch)
    await reconcile_task_tree(cfg, tree)
    assert tree.task_state(worker.id) == "completed"
    await _settle_parent(tree, manager, timeout=5.0, poll=0.02)
    await tree.completion.reopen_task(
        worker.id, request_id="reopen-1", reason="recheck the delivery",
        caller=OPERATOR)
    assert tree.task_state(worker.id) == "open"

    counts = _count_landing_git(monkeypatch)
    await reconcile_task_tree(cfg, tree)
    assert counts["git_verify_commit_landed"] >= 1, counts


@pytest.mark.asyncio
async def test_recovery_open_task_landing_unchanged(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """An open task's replayed review follow-up proves the landing and closes:
    one pass closes the task and delivers the completed report to the parent,
    exactly as the closed-task skip left it."""
    from src.core.task_recovery import reconcile_task_tree

    cfg, _session_mgr, tree, manager, worker = await _reviewed_implement_task(tmp_path, monkeypatch)
    counts = _count_landing_git(monkeypatch)
    await reconcile_task_tree(cfg, tree)
    assert counts["git_verify_commit_landed"] >= 1, counts
    assert counts["git_fetch"] >= 1, counts
    assert tree.task_state(worker.id) == "completed"
    reports = [e for e in tree.fact_history(manager.id) if e.get("type") == ET.CHILD_REPORT]
    assert reports and reports[-1]["outcome"] == "completed"
    await _settle_parent(tree, manager, timeout=5.0, poll=0.02)


@pytest.mark.asyncio
async def test_recovery_after_a_failed_review_picks_the_next_preference_backend(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """A failed review is a used attempt: recovery registers the next review
    on the next preference backend (the existing policy, unchanged)."""
    from conftest import backend_option

    from src.core.models import PatchSessionTaskRequest
    from src.core.task_recovery import reconcile_task_tree
    from tests.test_task_execution import init_repo_with_origin
    cfg, _session_mgr, tree, _manager, worker = await _manager_and_worker(tmp_path, monkeypatch)
    # Two reviewer entries beyond the worker's own backend: a failed first
    # attempt must move to the second one.
    cfg.backends.options.extend([
        backend_option(id="fake2", label="Fake2", type="cc-claude", model="fake2-model"),
        backend_option(id="fake3", label="Fake3", type="cc-claude", model="fake3-model"),
    ])
    cfg.backends.preference = ["fake2", "fake3"]
    repo, _origin = init_repo_with_origin(tmp_path / "repo")
    await tree.patch_task(
        worker.id,
        PatchSessionTaskRequest(
            task=TaskSpec(goal="do the work", repo_path=str(repo), task_type=TaskType.IMPLEMENT)),
        caller=OPERATOR)
    install_backends(
        monkeypatch, [SpawningScriptedBackend([result_event("review ok")])],
        WORKER_BUILD_BACKEND_PATCH_TARGET)
    patch_instructions_content(monkeypatch)
    run_id = "run-work"
    await tree.runs.register_run(
        RunRecord(id=run_id, session_id=worker.id, kind="work",
                  backend="fake", model="fake-model",
                  repo_path=str(repo), base_branch="main",
                  branch_name="task/work", worktree_path=str(repo)))
    await tree.dispatch.finish_run(worker.id, run_id, outcome="success", exit_code=0)
    # The first review attempt ran on the first preference backend and failed.
    failed_review = "review-failed"
    await tree.runs.register_run(
        RunRecord(id=failed_review, session_id=worker.id, kind="review",
                  review_of_run_id=run_id, backend="fake2", model="fake2-model"))
    await tree.dispatch.finish_run(worker.id, failed_review, outcome="failed", exit_code=1)

    await reconcile_task_tree(cfg, tree)
    reviews = [r for r in tree.runs.list_run_records_sync(worker.id) if r.kind == "review"]
    assert len(reviews) == 2, f"expected the retried review, got {[r.id for r in reviews]}"
    retried = next(r for r in reviews if r.id != failed_review)
    # The stable retry identity binds to the work run and its attempt number.
    from src.core.control_events import stable_run_id
    assert retried.id == stable_run_id(worker.id, f"review:{run_id}:2")
    assert retried.backend == "fake3", f"expected the next preference backend, got {retried.backend}"
    _run, outcome = await wait_for_terminal_run(tree, worker.id, retried.id)
    assert outcome == "success"
    # The retried review consumed the one scripted backend; nothing else launches.
    await reconcile_task_tree(cfg, tree)
    assert len([r for r in tree.runs.list_run_records_sync(worker.id) if r.kind == "review"]) == 2


@pytest.mark.asyncio
async def test_reconcile_replays_an_already_delivered_blocked_report_without_waking(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """The startup pass never wakes a parent for an already delivered report.

    The production wake-replay: an implement child whose landing check keeps
    failing (a rebase rewrote the base) re-derives the same stable blocked
    report id on every restart, and the pass replays every terminal review
    run. The first delivery woke the legacy parent once; the reconcile of the
    same state — two successful review runs, the report already in the
    parent's log — must append nothing and wake nobody.
    """
    from src.core.task_recovery import reconcile_task_tree

    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
    repo, _origin = init_repo_with_origin(tmp_path)
    tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
    legacy = await session_mgr.create_session(CreateSessionRequest(name="Legacy"), backend="fake")
    worker = await tree.create_task(
        request_id="w", task_parent_id=legacy.id, profile="worker",
        task=TaskSpec(goal="add a marker", repo_path=str(repo), base_branch="main",
                      task_type=TaskType.IMPLEMENT),
        name="W", backend=None, caller="operator")
    trigger, calls, fired = wake_probe()
    monkeypatch.setattr(MASTER_TRIGGER_TRIGGER_MASTER_PATCH_TARGET, trigger)
    patch_instructions_content(monkeypatch)

    # The reviewer never pushes, so the landing check fails on every pass.
    install_backends(
        monkeypatch,
        [SpawningScriptedBackend([result_event("implemented")],
                                 pre_run=partial(implement_marker_commit, tree, worker.id)),
         SpawningScriptedBackend([result_event("review ok")])],
        WORKER_BUILD_BACKEND_PATCH_TARGET)

    await tree.dispatch.admit_input(worker.id, event_type=ET.USER, content="Start the work.", actor="user")
    decision = await tree.dispatch.dispatch_pending(worker.id)
    _work, outcome = await wait_for_terminal_run(tree, worker.id, decision["run_id"])
    assert outcome == "success"

    deadline = asyncio.get_event_loop().time() + 15
    while asyncio.get_event_loop().time() < deadline:
        reports = [e for e in tree.events.load_events(legacy.id) if e.get("type") == ET.CHILD_REPORT]
        if reports:
            break
        await asyncio.sleep(0.05)
    else:
        pytest.fail("the review chain never reported to the legacy parent")
    await asyncio.wait_for(fired.wait(), timeout=5)
    assert len(calls) == 1
    assert [r["outcome"] for r in reports] == ["blocked"]
    assert tree.task_state(worker.id) == "open"

    # The shape that woke the production parent twice per restart: a second
    # successful review run of the same work run replays alongside the first.
    work_run = next(r for r in tree.runs.list_run_records_sync(worker.id) if r.kind == "work")
    await tree.runs.register_run(RunRecord(
        id="review-replay-2", session_id=worker.id, kind="review", review_of_run_id=work_run.id,
        backend="fake", model="fake-model", repo_path=work_run.repo_path,
        base_branch=work_run.base_branch, branch_name=work_run.branch_name))
    await tree.runs.record_finish(worker.id, "review-replay-2", "success")

    counters = await reconcile_task_tree(cfg, tree)
    await drain_legacy_wakes()

    assert counters["followups"] == 3  # the work run plus both review runs replayed
    assert len(calls) == 1
    reports = [e for e in tree.events.load_events(legacy.id) if e.get("type") == ET.CHILD_REPORT]
    assert len(reports) == 1
