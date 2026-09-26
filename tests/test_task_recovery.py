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
import json
from pathlib import Path

import pytest
from conftest import BUILD_BACKEND_PATCH_TARGET, patch_instructions_content

from src.core import event_types as ET
from src.core.models import RunRecord, TaskSpec, TaskType
from tests.test_task_execution import (
    SpawningScriptedBackend,
    _adapter_with_silent_broadcast,
    build_env,
    install_backends,
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

    monkeypatch.setattr("src.agents.worker.build_backend", fake_build)
    return builds


def drop_run_terminal_facts(cfg, tree, session_mgr, session_id: str, run_id: str) -> None:
    """Rewrite the session's event log without *run_id*'s terminal facts.

    Exactly the on-disk state a crash in the terminal-append window leaves:
    the launch, the translated stream, and the run metadata are durable; the
    run_finished fact is not.
    """
    from src.core.chat_events import chat_events_path
    events_path = chat_events_path(cfg.sessions_dir / session_id)
    facts = tree.events.load_events(session_id)
    kept = [e for e in facts
            if not (e.get("type") == ET.RUN_FINISHED and e.get("run_id") == run_id)]
    events_path.write_text("".join(json.dumps(e) + "\n" for e in kept), encoding="utf-8")
    # The events cache holds the pre-crash stream: drop the entry so readers
    # see exactly what the restarted server's disk carries.
    session_mgr._chat_events.clear_cache(session_id)


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
    cfg, session_mgr, tree, _manager, worker = await _manager_and_worker(tmp_path, monkeypatch)
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

    await reconcile_task_tree(cfg, tree, session_mgr)
    run, outcome = await wait_for_terminal_run(tree, worker.id, run_id)
    assert outcome == "success"
    assert run.pid == 424001
    assert run.input_event_ids == [str(admitted["id"])]
    assert len(builds) == 1
    # A repeated pass launches nothing new and duplicates nothing.
    await reconcile_task_tree(cfg, tree, session_mgr)
    assert len(tree.runs.list_run_records_sync(worker.id)) == 1
    assert len(builds) == 1


@pytest.mark.asyncio
async def test_restart_after_launch_reattaches_live_process(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """A recorded live (pid, pid_start) process is followed, never relaunched."""
    from src.core.task_recovery import reconcile_task_tree
    cfg, session_mgr, tree, _manager, worker = await _manager_and_worker(tmp_path, monkeypatch)
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
    await reconcile_task_tree(cfg, tree, session_mgr)
    _run, outcome = await wait_for_terminal_run(tree, worker.id, run_id)
    # The follow observed the recorded process ENDING (true→false) and the run
    # converged to its durable result instead of staying falsely running. The
    # scripted double never wrote a raw stream (no launcher drove it), so the
    # honest outcome is failed — never a relaunch, never a fabricated success.
    assert outcome in ("failed", "interrupted")
    assert len(builds) == 0
    assert len(tree.runs.list_run_records_sync(worker.id)) == 1


@pytest.mark.asyncio
async def test_dead_process_drains_to_its_durable_result(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """A process that ended before its terminal fact lands converges to the
    raw stream's result — never stuck 'running', never relaunched."""
    from src.core.task_recovery import reconcile_task_tree
    cfg, session_mgr, tree, _manager, worker = await _manager_and_worker(tmp_path, monkeypatch)
    backend = SpawningScriptedBackend([result_event("finished before the crash")])
    builds = install_resume_ready_backends(monkeypatch, [backend])
    run_id = "run-dead"
    await tree.runs.register_run(
        RunRecord(id=run_id, session_id=worker.id, kind="work",
                  backend="fake", model="fake-model"))
    await tree.dispatch.admit_input(
        worker.id, event_type=ET.USER, content="Start the task.", actor="user")
    # The crash happened after the reservation bound its batch (the registration
    # window), so the drain's re-landed fact acknowledges exactly this input.
    await tree.dispatch.claim_input_batch_locked(worker.id, run_id)
    decision = await tree.dispatch.dispatch_pending(worker.id)
    assert decision["launch"] is True and decision["run_id"] == run_id
    await wait_for_terminal_run(tree, worker.id, run_id)
    # Simulate the crash in the terminal-append window: the stream is durable,
    # the run_finished fact is not. (The raw bytes below are what the real
    # backend writes; the double only translates.)
    drop_run_terminal_facts(cfg, tree, session_mgr, worker.id, run_id)
    raw = tree.runs.run_dir(worker.id, run_id) / "agent.raw.ndjson"
    raw.write_text(json.dumps(result_event("finished before the crash")) + "\n", encoding="utf-8")
    assert tree.runs.terminal_outcome(
        tree.runs.load_events_sync(worker.id), run_id) is None, "the crash window was not simulated"

    await reconcile_task_tree(cfg, tree, session_mgr)
    _run, outcome = await wait_for_terminal_run(tree, worker.id, run_id)
    # The ended process converged to its durable result from the raw stream.
    assert outcome == "success"
    assert len(builds) == 1  # drained, never relaunched
    # Repeated recovery is idempotent.
    await reconcile_task_tree(cfg, tree, session_mgr)
    assert len(tree.runs.list_run_records_sync(worker.id)) == 1
    assert len(builds) == 1


@pytest.mark.asyncio
async def test_stop_request_wins_over_launch_and_recovery(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    from src.core.task_recovery import reconcile_task_tree
    cfg, session_mgr, tree, _manager, worker = await _manager_and_worker(tmp_path, monkeypatch)
    builds = install_backends(monkeypatch, [SpawningScriptedBackend([result_event("x")])],
                              "src.agents.worker.build_backend")
    patch_instructions_content(monkeypatch)
    run_id = "run-stopped"
    await tree.runs.register_run(
        RunRecord(id=run_id, session_id=worker.id, kind="work",
                  backend="fake", model="fake-model"))
    await tree.dispatch.claim_input_batch_locked(worker.id, run_id)
    await tree.runs.request_stop(worker.id, run_id, "stop-1")
    await reconcile_task_tree(cfg, tree, session_mgr)
    # Stop precedence: the queued run never launches and never claims input;
    # the durable stop request stands and no side effect happened.
    events = tree.runs.load_events_sync(worker.id)
    assert tree.runs.stop_requested(events, run_id)
    assert builds == []
    stopped_run = await tree.runs.get_run(worker.id, run_id)
    assert stopped_run is not None and stopped_run.pid is None
    await reconcile_task_tree(cfg, tree, session_mgr)
    assert builds == []


@pytest.mark.asyncio
async def test_terminal_append_crash_replays_review_and_close_once(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """A work run carrying a terminal fact whose follow-up never ran (crash
    between finish and the review chain): recovery spawns the review exactly
    once, and a repeated pass spawns nothing more."""
    from src.core.models import PatchSessionTaskRequest
    from src.core.run_token import CallerIdentity
    from src.core.task_recovery import reconcile_task_tree
    from tests.test_task_execution import init_repo_with_origin
    cfg, session_mgr, tree, _manager, worker = await _manager_and_worker(tmp_path, monkeypatch)
    repo, _origin = init_repo_with_origin(tmp_path / "repo")
    await tree.patch_task(
        worker.id,
        PatchSessionTaskRequest(
            task=TaskSpec(goal="do the work", repo_path=str(repo), task_type=TaskType.IMPLEMENT)),
        caller=CallerIdentity(kind="operator"))
    backend = SpawningScriptedBackend([result_event("work done")])
    install_backends(monkeypatch, [backend], "src.agents.worker.build_backend")
    patch_instructions_content(monkeypatch)
    run_id = "run-work"
    await tree.runs.register_run(
        RunRecord(id=run_id, session_id=worker.id, kind="work",
                  backend="fake", model="fake-model",
                  repo_path=str(repo), base_branch="main",
                  branch_name="task/work", worktree_path=str(repo)))
    # The terminal fact landed (crash before the review chain ran).
    await tree.dispatch.finish_run(worker.id, run_id, outcome="success", exit_code=0)

    await reconcile_task_tree(cfg, tree, session_mgr)
    records = tree.runs.list_run_records_sync(worker.id)
    reviews = [r for r in records if r.kind == "review"]
    assert len(reviews) == 1, "the review follow-up was not repaired"
    assert reviews[0].review_of_run_id == run_id
    # A repeated pass never spawns a second review.
    await reconcile_task_tree(cfg, tree, session_mgr)
    reviews2 = [r for r in tree.runs.list_run_records_sync(worker.id) if r.kind == "review"]
    assert len(reviews2) == 1


@pytest.mark.asyncio
async def test_terminal_append_crash_replays_parent_failure_report_once(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """A failed work run whose parent report never landed: recovery delivers it
    exactly once (the stable report id dedups a repeated pass)."""
    from src.core.task_recovery import reconcile_task_tree
    cfg, session_mgr, tree, manager, worker = await _manager_and_worker(tmp_path, monkeypatch)
    run_id = "run-failed"
    await tree.runs.register_run(
        RunRecord(id=run_id, session_id=worker.id, kind="work",
                  backend="fake", model="fake-model"))
    await tree.dispatch.finish_run(worker.id, run_id, outcome="failed", exit_code=1)

    await reconcile_task_tree(cfg, tree, session_mgr)
    reports = [e for e in tree.events.load_events(manager.id) if e.get("type") == ET.CHILD_REPORT]
    assert len(reports) == 1
    assert reports[0].get("outcome") == "failed"
    # Repeated recovery: no duplicate report.
    await reconcile_task_tree(cfg, tree, session_mgr)
    reports2 = [e for e in tree.events.load_events(manager.id) if e.get("type") == ET.CHILD_REPORT]
    assert len(reports2) == 1


@pytest.mark.asyncio
async def test_manager_close_request_replays_once(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """A manager's owner-Run close request saved durably, its consuming recheck
    lost to a crash: recovery re-evaluates it once; repeats never double-close."""
    from src.core.run_token import CallerIdentity, RunTokenClaims
    from src.core.task_completion import CompletionEvidence
    from src.core.task_recovery import reconcile_task_tree
    cfg, session_mgr, tree, manager = await _takeoff_manager(tmp_path, monkeypatch)
    run_id = "run-mgr"
    await tree.runs.register_run(
        RunRecord(id=run_id, session_id=manager.id, kind="manager_turn",
                  backend="fake", model="fake-model"))
    import os as _os

    from src.core.runs import read_pid_stat
    live_pid = _os.getpid()
    live_start = read_pid_stat(live_pid)[0]
    await tree.runs.record_launch(manager.id, run_id, pid=live_pid, pid_start=live_start)
    # The close REQUEST lands durably while the run is still active; the
    # owner's recheck consumes it once the run reaches its terminal fact. The
    # crash window: the terminal fact is durable, the recheck never ran.
    await tree.completion.complete_task(
        manager.id, request_id="auto-close", caller=CallerIdentity(
            kind="agent", claims=RunTokenClaims(
                session_id=manager.id, run_id=run_id, agent="PM")),
        evidence=CompletionEvidence(summary="all done", run_ids=[run_id],
                                    result_refs=[f"run:{run_id}"]))
    await tree.dispatch.finish_run(manager.id, run_id, outcome="success", exit_code=0)
    closed = [e for e in tree.events.load_events(manager.id) if e.get("type") == ET.TASK_CLOSED]
    assert len(closed) == 1, f"the owner close did not land: {closed}"
    # Recovery re-runs the recheck for the terminal run: the stable request id
    # replay keeps exactly one close — no duplicate parent turn, no double close.
    await reconcile_task_tree(cfg, tree, session_mgr)
    assert len([e for e in tree.events.load_events(manager.id)
                if e.get("type") == ET.TASK_CLOSED]) == 1
    # A repeated pass stays idempotent.
    await reconcile_task_tree(cfg, tree, session_mgr)
    assert len([e for e in tree.events.load_events(manager.id)
                if e.get("type") == ET.TASK_CLOSED]) == 1


@pytest.mark.asyncio
async def test_recovery_scopes_to_this_instance_only(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """A second synthetic instance's data and owned process are unaffected."""
    from src.core.task_recovery import reconcile_task_tree
    cfg, session_mgr, tree, _manager, _worker = await _manager_and_worker(tmp_path, monkeypatch)
    # A second instance under the same tmp tree: its own home, its own live run.
    from src.core.config import CharlieBotConfig
    other_home = tmp_path / "other-home"
    other_cfg = CharlieBotConfig(
        charliebot_home=other_home,
        backends={"options": cfg.backends.options},
        paths={"worktree_dir": str(other_home / "worktrees")})
    from src.core.sessions import SessionManager as SM
    from src.core.task_sessions import TaskTreeManager as TTM
    other_session_mgr = SM(other_cfg)
    other_tree = TTM(other_cfg, other_session_mgr)
    other_manager = await other_tree.create_task(
        request_id="root", task_parent_id=None, profile="manager",
        task=TaskSpec(goal="other"), name="OTHER", backend=None, caller="operator")
    patch_instructions_content(monkeypatch)
    install_backends(monkeypatch, [SpawningScriptedBackend([result_event("other takes off")])],
                     BUILD_BACKEND_PATCH_TARGET)
    other_tree.dispatch.executor = _adapter_with_silent_broadcast(other_cfg, other_session_mgr, other_tree, monkeypatch)
    await other_tree.dispatch.admit_input(
        other_manager.id, event_type=ET.USER, content="Take off.", actor="user")
    decision = await other_tree.dispatch.dispatch_pending(other_manager.id)
    assert decision["launch"] is True
    other_run_id = decision["run_id"]
    deadline = asyncio.get_event_loop().time() + 10
    while asyncio.get_event_loop().time() < deadline:
        other_run = await other_tree.runs.get_run(other_manager.id, other_run_id)
        if other_run is not None and other_run.pid is not None:
            break
        await asyncio.sleep(0.05)
    assert other_run is not None and other_run.pid is not None

    # THIS instance's recovery never touches the other instance's records: the
    # other instance's owned process and event log are untouched.
    other_events_before = len(other_tree.events.load_events(other_manager.id))
    await reconcile_task_tree(cfg, tree, session_mgr)
    other_run = await other_tree.runs.get_run(other_manager.id, other_run_id)
    assert other_run is not None and other_run.pid is not None
    assert len(other_tree.events.load_events(other_manager.id)) == other_events_before
    # ...and the other instance's own recovery still reconciles its own work.
    await reconcile_task_tree(other_cfg, other_tree, other_session_mgr)
    other_run2 = await other_tree.runs.get_run(other_manager.id, other_run_id)
    assert other_run2 is not None and other_run2.pid is not None


@pytest.mark.asyncio
async def test_terminal_append_crash_replays_automatic_completion_once(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """A successful non-implement work run whose terminal fact landed while the
    completion follow-up (close + parent report) was lost to a crash: recovery
    replays it exactly once — the task closes and the parent gets one report."""
    from src.core.task_recovery import reconcile_task_tree
    cfg, session_mgr, tree, manager, worker = await _manager_and_worker(
        tmp_path, monkeypatch, task_type=TaskType.QUICK_EDIT)
    run_id = "run-quickedit"
    await tree.runs.register_run(
        RunRecord(id=run_id, session_id=worker.id, kind="work",
                  backend="fake", model="fake-model"))
    # The crash window: the durable terminal fact exists; after_run_finished
    # (the automatic completion and the parent report) never ran.
    async with tree.control_lock:
        await tree.runs.record_finish_locked(worker.id, run_id, "success", exit_code=0)
    assert tree.task_state(worker.id) == "open"
    assert not [e for e in tree.events.load_events(manager.id) if e.get("type") == ET.CHILD_REPORT]

    await reconcile_task_tree(cfg, tree, session_mgr)
    assert tree.task_state(worker.id) == "completed"
    reports = [e for e in tree.events.load_events(manager.id) if e.get("type") == ET.CHILD_REPORT]
    assert len(reports) == 1
    assert reports[0].get("outcome") == "completed"
    # A repeated pass lands nothing twice: one close, one report.
    await reconcile_task_tree(cfg, tree, session_mgr)
    closed = [e for e in tree.events.load_events(worker.id) if e.get("type") == ET.TASK_CLOSED]
    assert len(closed) == 1
    reports2 = [e for e in tree.events.load_events(manager.id) if e.get("type") == ET.CHILD_REPORT]
    assert len(reports2) == 1


@pytest.mark.asyncio
async def test_live_resume_attaches_without_blocking_the_startup_pass(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """A recorded live run's resume follow must not hold the recovery pass
    (the server lifespan awaits it): the pass returns with the follow
    attached, and the follow converges when the process actually ends."""
    from src.core.task_recovery import reconcile_task_tree
    cfg, session_mgr, tree, _manager, worker = await _manager_and_worker(tmp_path, monkeypatch)
    builds = install_resume_ready_backends(monkeypatch, [])
    gate = asyncio.Event()  # the recorded process stays "alive" until released
    import src.core.runs as runs_mod
    monkeypatch.setattr(runs_mod, "is_run_alive", lambda *a, **k: not gate.is_set())
    run_id = "run-live-block"
    await tree.runs.register_run(
        RunRecord(id=run_id, session_id=worker.id, kind="work",
                  backend="fake", model="fake-model"))
    await tree.runs.record_launch(worker.id, run_id, pid=424888, pid_start="1-424000")

    # The pass returns even though the recorded process is still alive — an
    # inline await would hold startup until the run's process ends.
    await asyncio.wait_for(reconcile_task_tree(cfg, tree, session_mgr), timeout=10)
    run = await tree.runs.get_run(worker.id, run_id)
    assert run is not None
    assert tree.runs.terminal_outcome(
        tree.runs.load_events_sync(worker.id), run_id) is None
    # No competing process: the recorded identity holds the serialized slot.
    assert builds == []

    # The attached follow converges in the background when the process ends.
    gate.set()
    run, outcome = await wait_for_terminal_run(tree, worker.id, run_id)
    assert outcome in ("failed", "interrupted")
