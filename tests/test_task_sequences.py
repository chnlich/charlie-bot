"""Improve-sequence tests: the v2 improve loop is one worker child with
iteration Runs, one final report, and no automatic whole-loop restart.

Exercises the actual /api/internal/improve entry point against synthetic
instances with deterministic scripted backend processes: two iterations stay
one child, a live goal change steers the next iteration, the success/stop and
quota outcomes are truthful, and a restart marks the interrupted controller
honestly without resuming the loop or leaving a permanently blocking lock.
"""

from __future__ import annotations

import asyncio
from pathlib import Path

import pytest
from conftest import patch_instructions_content

from src.core import event_types as ET
from src.core.improve_command import load_loop_state
from src.core.models import SessionMetadata
from tests.test_task_execution import (
    OPERATOR,
    SpawningScriptedBackend,
    _adapter_with_silent_broadcast,
    build_env,
    init_repo_with_origin,
    install_backends,
    make_api_client,
    result_event,
    stub_credentials,
)


def _worker_backends(monkeypatch, outcomes: list[str]) -> list:
    """One scripted worker per iteration build, in launch order."""
    return install_backends(
        monkeypatch,
        [SpawningScriptedBackend([result_event(text)]) for text in outcomes],
        "src.agents.worker.build_backend")


async def _start_loop(cfg, session_mgr, tree, manager, monkeypatch, payload_overrides=None,
                      wait_effect=None):
    """POST the improve loop against the v2 manager and wait for the controller's child."""
    patch_instructions_content(monkeypatch)
    stub_credentials(monkeypatch, {"charliebot": {"access_key": "op-secret"}})
    monkeypatch.setenv("CHARLIEBOT_HOME", str(cfg.charliebot_home))
    tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
    payload = {
        "session_id": manager.id,
        "goal": "## Goal\n\nimprove the thing\n",
        "iterations": 2,
        "repo_path": str(tmp_repo),
        "base_branch": "main",
        "work_branch": "improve/test-branch",
    }
    payload.update(payload_overrides or {})
    with make_api_client(cfg, session_mgr, tree) as client:
        resp = client.post("/api/internal/improve", json=payload, headers=OPERATOR)
        assert resp.status_code == 200, resp.text
        body = resp.json()
        assert body["status"] == "started"
        child_id = body["child_session_id"]
        # The controller task is born inside the client's request loop; every
        # wait below happens while that loop is still alive.
        if wait_effect is not None:
            await wait_effect(client, body)
        else:
            deadline = asyncio.get_event_loop().time() + 10
            while asyncio.get_event_loop().time() < deadline:
                meta = await tree.load_meta(child_id)
                if meta is not None and tree.runs.list_run_records_sync(child_id):
                    break
                await asyncio.sleep(0.05)
            else:
                pytest.fail("the sequence controller never registered its first iteration run")
    return body, child_id


tmp_repo: Path = Path("/")  # replaced per-test by the fixture


@pytest.fixture()
def repo(tmp_path: Path) -> Path:
    global tmp_repo
    tmp_repo, _origin = init_repo_with_origin(tmp_path / "improve-repo")
    return tmp_repo


@pytest.mark.asyncio
async def test_two_iterations_stay_one_child_with_ordered_runs(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
    """Two iterations: ONE child, two ordered iteration Runs, one final report."""
    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
    from src.core.models import TaskSpec
    manager = await tree.create_task(
        request_id="root", task_parent_id=None, profile="manager",
        task=TaskSpec(goal="pm"), name="PM", backend=None, caller="operator")
    builds = _worker_backends(monkeypatch, ["iter one words", "iter two words"])

    await tree.dispatch.admit_input(
        manager.id, event_type=ET.USER,
        content="Take off. Run the improve loop.", actor="user")
    async def _wait_done(client, body):
        child_id = body["child_session_id"]
        deadline = asyncio.get_event_loop().time() + 30
        while asyncio.get_event_loop().time() < deadline:
            records = tree.runs.list_run_records_sync(child_id)
            reports = [e for e in tree.events.load_events(manager.id)
                       if e.get("type") == ET.CHILD_REPORT]
            if len(records) == 2 and reports and all(
                    tree.runs.terminal_outcome(tree.runs.load_events_sync(child_id), r.id)
                    is not None for r in records):
                return
            await asyncio.sleep(0.1)
        pytest.fail(f"the sequence never finished: runs={[(r.id, r.kind) for r in tree.runs.list_run_records_sync(child_id)]} "
                    f"reports={[e for e in tree.events.load_events(manager.id) if e.get('type') == ET.CHILD_REPORT]}")

    body, child_id = await _start_loop(
        cfg, session_mgr, tree, manager, monkeypatch, wait_effect=_wait_done)

    records = tree.runs.list_run_records_sync(child_id)
    assert [r.kind for r in records] == ["iteration", "iteration"]
    positions = [r.sequence_ref.position for r in records]
    kinds = [r.sequence_ref.kind for r in records]
    assert positions == [1, 2] and kinds == ["improve", "improve"]
    assert all(str(repo) in r.sequence_ref.owner_ref for r in records) or all(
        "loops" in r.sequence_ref.owner_ref for r in records)
    # ONE child only: the loop never created a second task under the manager.
    metas = [SessionMetadata.model_validate_json(p.read_text())
             for p in sorted((cfg.sessions_dir).glob("*/metadata.json"))]
    assert [m.id for m in metas if m.task_parent_id == manager.id] == [child_id]
    # Both iteration prompts carry the worker memory block + iteration report
    # instructions (the existing worker prompt builder, loop_dir context).
    assert len(builds) == 2
    for i, b in enumerate(builds, start=1):
        assert f"iter_{i:04d}.md" in b["backend"].prompt
    # One final result report on the manager, from the child, after BOTH runs.
    deadline = asyncio.get_event_loop().time() + 10
    report = None
    while asyncio.get_event_loop().time() < deadline:
        events = tree.events.load_events(manager.id)
        reports = [e for e in events if e.get("type") == ET.CHILD_REPORT]
        if reports:
            report = reports[0]
            break
        await asyncio.sleep(0.1)
    assert report is not None, "the final sequence result was never delivered"
    assert report.get("child_session_id") == child_id
    assert report.get("outcome") in ("completed", "blocked", "failed", "cancelled")
    assert "Improve loop" in str(report.get("summary"))
    # The loop state is honestly 'blocked' — iterations exhausted without a
    # proven landing (no merge-back) is NOT successful delivery.
    state = await load_loop_state(manager.id, body["loop_id"], cfg)
    assert state is not None and state.status == "blocked"


@pytest.mark.asyncio
async def test_live_goal_change_affects_next_iteration(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
    """The controller re-reads goal.md every iteration: a mid-loop edit steers the next one."""
    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
    from src.core.models import TaskSpec
    manager = await tree.create_task(
        request_id="root", task_parent_id=None, profile="manager",
        task=TaskSpec(goal="pm"), name="PM", backend=None, caller="operator")
    class _EditBetween(SpawningScriptedBackend):
        async def run(self, prompt, cwd, env, uploaded_files=None):
            self.prompt = prompt
            self.env = dict(env)
            self._pid += 1
            if self._on_spawn is not None:
                await self._on_spawn(self._pid)
            for event in self._events:
                if self.terminated:
                    return
                yield event
            # After the first build's scripted stream, edit the live goal so the
            # second iteration must see it.
            goal_path = cfg.sessions_dir / manager.id / "loops" / "1" / "goal.md"
            goal_path.write_text("## Goal\n\nnow improve the OTHER thing\n")

    first = _EditBetween([result_event("one")])
    second = SpawningScriptedBackend([result_event("two")])
    queue = [first, second]
    monkeypatch.setattr("src.agents.worker.build_backend", lambda *a, **k: queue.pop(0))
    patch_instructions_content(monkeypatch)
    stub_credentials(monkeypatch, {"charliebot": {"access_key": "op-secret"}})
    monkeypatch.setenv("CHARLIEBOT_HOME", str(cfg.charliebot_home))
    tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)

    await tree.dispatch.admit_input(
        manager.id, event_type=ET.USER, content="Take off. Run the improve loop.", actor="user")
    with make_api_client(cfg, session_mgr, tree) as client:
        resp = client.post("/api/internal/improve", json={
            "session_id": manager.id,
            "goal": "## Goal\n\nimprove the thing\n",
            "iterations": 2,
            "repo_path": str(repo),
            "base_branch": "main",
            "work_branch": "improve/live-goal",
        }, headers=OPERATOR)
        assert resp.status_code == 200, resp.text
        child_id = resp.json()["child_session_id"]

        deadline = asyncio.get_event_loop().time() + 30
        while asyncio.get_event_loop().time() < deadline:
            records = tree.runs.list_run_records_sync(child_id)
            reports = [e for e in tree.events.load_events(manager.id)
                       if e.get("type") == ET.CHILD_REPORT]
            if len(records) == 2 and reports and all(
                    tree.runs.terminal_outcome(tree.runs.load_events_sync(child_id), r.id)
                    is not None for r in records):
                break
            await asyncio.sleep(0.1)
        else:
            pytest.fail("both iterations never finished")
    # The second build's prompt carries the EDITED goal, not the original one.
    assert second.prompt is not None
    assert "now improve the OTHER thing" in second.prompt
    assert "improve the thing" not in second.prompt.split("Previous iteration summaries")[0]


@pytest.mark.asyncio
async def test_user_stop_prevents_further_iterations_and_reports_cancelled(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
    """Stop/cancel retains evidence and prevents further iterations."""
    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
    from src.core.improve_command import stop_improve_loop
    from src.core.models import TaskSpec
    manager = await tree.create_task(
        request_id="root", task_parent_id=None, profile="manager",
        task=TaskSpec(goal="pm"), name="PM", backend=None, caller="operator")

    class _StopAfterFirst(SpawningScriptedBackend):
        async def run(self, prompt, cwd, env, uploaded_files=None):
            self.prompt = prompt
            self.env = dict(env)
            self._pid += 1
            if self._on_spawn is not None:
                await self._on_spawn(self._pid)
            for event in self._events:
                if self.terminated:
                    return
                yield event
            # The user's stop lands after this iteration's work: the controller
            # must honor it before spawning any further iteration.
            await stop_improve_loop(manager.id, cfg)

    monkeypatch.setattr(
        "src.agents.worker.build_backend", lambda *a, **k: _StopAfterFirst([result_event("one")]))
    patch_instructions_content(monkeypatch)
    stub_credentials(monkeypatch, {"charliebot": {"access_key": "op-secret"}})
    monkeypatch.setenv("CHARLIEBOT_HOME", str(cfg.charliebot_home))
    tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)

    await tree.dispatch.admit_input(
        manager.id, event_type=ET.USER, content="Take off. Run the improve loop.", actor="user")
    with make_api_client(cfg, session_mgr, tree) as client:
        resp = client.post("/api/internal/improve", json={
            "session_id": manager.id, "goal": "## Goal\n\nimprove the thing\n",
            "iterations": 3, "repo_path": str(repo), "base_branch": "main",
            "work_branch": "improve/stop-me",
        }, headers=OPERATOR)
        assert resp.status_code == 200, resp.text
        child_id = resp.json()["child_session_id"]
        loop_id = resp.json()["loop_id"]

        deadline = asyncio.get_event_loop().time() + 30
        while asyncio.get_event_loop().time() < deadline:
            reports = [e for e in tree.events.load_events(manager.id)
                       if e.get("type") == ET.CHILD_REPORT]
            if reports:
                break
            await asyncio.sleep(0.1)
        else:
            pytest.fail("the final report never arrived")
    # Exactly ONE iteration ran; the stop prevented the rest.
    assert len(tree.runs.list_run_records_sync(child_id)) == 1
    state = await load_loop_state(manager.id, loop_id, cfg)
    assert state is not None and state.status == "stopped"
    report = [e for e in tree.events.load_events(manager.id)
              if e.get("type") == ET.CHILD_REPORT][0]
    assert "stopped by user" in str(report.get("summary"))
    # The loop's evidence (report file) is retained.
    loop_dir = cfg.sessions_dir / manager.id / "loops" / str(loop_id)
    assert (loop_dir / "goal.md").is_file()
    # And a fresh loop can start again afterwards (the lock was cleared).
    with make_api_client(cfg, session_mgr, tree) as client:
        resp = client.post("/api/internal/improve", json={
            "session_id": manager.id, "goal": "## Goal\n\nimprove the thing\n",
            "iterations": 1, "repo_path": str(repo), "base_branch": "main",
            "work_branch": "improve/after-stop",
        }, headers=OPERATOR)
        assert resp.status_code == 200, resp.text


@pytest.mark.asyncio
async def test_quota_blocked_iteration_fails_loop_without_further_iterations(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
    """A quota blocker surfaces as a truthful failed outcome; no later iteration spawns."""
    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
    from src.core.models import TaskSpec
    manager = await tree.create_task(
        request_id="root", task_parent_id=None, profile="manager",
        task=TaskSpec(goal="pm"), name="PM", backend=None, caller="operator")

    quota_event = {
        "type": "error",
        "message": "Error: quota exceeded. Your limit will reset at 5pm (Asia/Shanghai)",
    }
    monkeypatch.setattr(
        "src.agents.worker.build_backend",
        lambda *a, **k: SpawningScriptedBackend([quota_event, result_event("ignored")], exit_code=1))
    patch_instructions_content(monkeypatch)
    stub_credentials(monkeypatch, {"charliebot": {"access_key": "op-secret"}})
    monkeypatch.setenv("CHARLIEBOT_HOME", str(cfg.charliebot_home))
    tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)

    await tree.dispatch.admit_input(
        manager.id, event_type=ET.USER, content="Take off. Run the improve loop.", actor="user")
    with make_api_client(cfg, session_mgr, tree) as client:
        resp = client.post("/api/internal/improve", json={
            "session_id": manager.id, "goal": "## Goal\n\nimprove the thing\n",
            "iterations": 3, "repo_path": str(repo), "base_branch": "main",
            "work_branch": "improve/quota",
        }, headers=OPERATOR)
        assert resp.status_code == 200, resp.text
        child_id = resp.json()["child_session_id"]
        loop_id = resp.json()["loop_id"]

        deadline = asyncio.get_event_loop().time() + 30
        while asyncio.get_event_loop().time() < deadline:
            reports = [e for e in tree.events.load_events(manager.id)
                       if e.get("type") == ET.CHILD_REPORT]
            if reports:
                break
            await asyncio.sleep(0.1)
        else:
            pytest.fail("the blocked report never arrived")
    # One iteration ran, the quota blocker was judged from ITS events log, and
    # the loop stopped as failed — never reported as a successful delivery.
    assert len(tree.runs.list_run_records_sync(child_id)) == 1
    state = await load_loop_state(manager.id, loop_id, cfg)
    assert state is not None and state.status == "failed"
    report = [e for e in tree.events.load_events(manager.id)
              if e.get("type") == ET.CHILD_REPORT][0]
    assert "blocked" in str(report.get("summary")).lower() or "usage limit" in str(report.get("summary")).lower()


@pytest.mark.asyncio
async def test_restart_marks_interrupted_controller_without_resuming(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
    """After a restart the launched iteration reconciles, the loop does NOT resume,
    the state is honest, and the stale lock cannot block the next loop forever."""
    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
    from src.core.improve_command import _active_loop_path
    from src.core.improve_sequence import reconcile_interrupted_sequences
    from src.core.models import TaskSpec
    manager = await tree.create_task(
        request_id="root", task_parent_id=None, profile="manager",
        task=TaskSpec(goal="pm"), name="PM", backend=None, caller="operator")

    class _HangForever(SpawningScriptedBackend):
        async def run(self, prompt, cwd, env, uploaded_files=None):
            self.prompt = prompt
            self.env = dict(env)
            self._pid += 1
            if self._on_spawn is not None:
                await self._on_spawn(self._pid)  # the launch is recorded...
            await asyncio.Event().wait()  # ...and the process never completes
            yield {}  # pragma: no cover  (unreachable: makes this an async generator)

    def _hang_build(*_a, **kwargs):
        backend = _HangForever([result_event("x")])
        on_spawn = kwargs.get("on_spawn")
        if on_spawn is not None:
            backend.set_on_spawn(on_spawn)
        return backend

    monkeypatch.setattr("src.agents.worker.build_backend", _hang_build)
    patch_instructions_content(monkeypatch)
    stub_credentials(monkeypatch, {"charliebot": {"access_key": "op-secret"}})
    monkeypatch.setenv("CHARLIEBOT_HOME", str(cfg.charliebot_home))
    tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)

    await tree.dispatch.admit_input(
        manager.id, event_type=ET.USER, content="Take off. Run the improve loop.", actor="user")
    with make_api_client(cfg, session_mgr, tree) as client:
        resp = client.post("/api/internal/improve", json={
            "session_id": manager.id, "goal": "## Goal\n\nimprove the thing\n",
            "iterations": 3, "repo_path": str(repo), "base_branch": "main",
            "work_branch": "improve/restart",
        }, headers=OPERATOR)
        assert resp.status_code == 200, resp.text
        child_id = resp.json()["child_session_id"]
        loop_id = resp.json()["loop_id"]

        deadline = asyncio.get_event_loop().time() + 20
        while asyncio.get_event_loop().time() < deadline:
            records = tree.runs.list_run_records_sync(child_id)
            if records and records[0].pid is not None:
                break
            await asyncio.sleep(0.05)
        else:
            pytest.fail("the first iteration never launched")

    # The restart: the controller task is gone (a new reconciliation pass with
    # a fresh pid finds the loop stamped by the dead one).
    state = await load_loop_state(manager.id, loop_id, cfg)
    assert state is not None and state.status == "running"
    state.server_pid = 999999  # the dead predecessor process
    from src.core.improve_command import save_loop_state
    await save_loop_state(manager.id, state, cfg)

    repaired = await reconcile_interrupted_sequences(cfg, tree, boot_pid=12345)
    assert repaired == 1
    state = await load_loop_state(manager.id, loop_id, cfg)
    assert state is not None and state.status == "interrupted"
    assert not _active_loop_path(manager.id, cfg).exists()
    # The chat stream carries the honest notice (never a fabricated success).
    events = tree.events.load_events(manager.id)
    notices = [e for e in events if e.get("type") == ET.IMPROVE_FAILED
               and "interrupted" in str(e.get("error", ""))]
    assert notices, "the interrupted controller was not reported"
    # The launched iteration itself is reconciled by the Run recovery pass:
    # with the backend double holding forever, the drained outcome is decided
    # by the adapter's follow (tested in test_task_recovery); here the loop
    # state and lock are what the sequence boundary owes.
    # A new loop can start immediately (no permanently blocking active.lock).
    with make_api_client(cfg, session_mgr, tree) as client:
        resp = client.post("/api/internal/improve", json={
            "session_id": manager.id, "goal": "## Goal\n\nimprove the thing\n",
            "iterations": 1, "repo_path": str(repo), "base_branch": "main",
            "work_branch": "improve/after-restart",
        }, headers=OPERATOR)
        assert resp.status_code == 200, resp.text


@pytest.mark.asyncio
async def test_replayed_finalization_does_not_duplicate_child_or_iterations(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
    """Stable operation identity: re-registering the child and its iteration
    Runs (a replayed admission / a repeated finalization) binds to the same records."""
    from src.core.control_events import stable_run_id
    from src.core.improve_sequence import create_improve_child, iteration_run_request_id, register_iteration_run
    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
    from src.core.models import TaskSpec
    manager = await tree.create_task(
        request_id="root", task_parent_id=None, profile="manager",
        task=TaskSpec(goal="pm"), name="PM", backend=None, caller="operator")

    child1 = await create_improve_child(
        tree, manager.id, 7, "goal", repo_path=str(repo), base_branch=None)
    child2 = await create_improve_child(
        tree, manager.id, 7, "goal", repo_path=str(repo), base_branch=None)
    assert child1.id == child2.id

    run1 = await register_iteration_run(
        tree, child1.id, manager.id, 7, 1, cfg,
        resolved_backend="fake", resolved_model="fake-model",
        repo_path=str(repo), base_branch="main", work_branch="improve/x",
        worktree_path=str(tmp_path / "wt"))
    run2 = await register_iteration_run(
        tree, child1.id, manager.id, 7, 1, cfg,
        resolved_backend="fake", resolved_model="fake-model",
        repo_path=str(repo), base_branch="main", work_branch="improve/x",
        worktree_path=str(tmp_path / "wt"))
    assert run1.id == run2.id
    assert run1.id == stable_run_id(child1.id, iteration_run_request_id(7, 1))
    assert run1.sequence_ref.kind == "improve"
    assert run1.sequence_ref.position == 1
    assert len(tree.runs.list_run_records_sync(child1.id)) == 1


@pytest.mark.asyncio
async def test_invalid_backend_fails_admission_without_leaking_the_lock(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
    """An unknown requested backend is a clean 400 BEFORE the reservation: no
    leaked active.lock, no "running" state stamped with the live pid, and a
    valid retry is not blocked by the failed admission."""
    from src.core.improve_command import _active_loop_path, _loops_dir, find_running_loop
    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
    from src.core.models import TaskSpec
    manager = await tree.create_task(
        request_id="root", task_parent_id=None, profile="manager",
        task=TaskSpec(goal="pm"), name="PM", backend=None, caller="operator")
    patch_instructions_content(monkeypatch)
    stub_credentials(monkeypatch, {"charliebot": {"access_key": "op-secret"}})
    monkeypatch.setenv("CHARLIEBOT_HOME", str(cfg.charliebot_home))
    await tree.dispatch.admit_input(
        manager.id, event_type=ET.USER, content="Take off. Run the improve loop.", actor="user")
    payload = {
        "session_id": manager.id,
        "goal": "## Goal\n\nimprove the thing\n",
        "iterations": 1,
        "repo_path": str(repo),
        "base_branch": "main",
        "backend": "does-not-exist",
    }
    with make_api_client(cfg, session_mgr, tree) as client:
        resp = client.post("/api/internal/improve", json=payload, headers=OPERATOR)
    assert resp.status_code == 400, resp.text
    assert not _active_loop_path(manager.id, cfg).exists()
    assert await find_running_loop(manager.id, cfg) is None
    # The reservation never ran: no loop directory state exists at all.
    assert not _loops_dir(manager.id, cfg).is_dir()

    # The failed admission blocks nothing: the valid retry starts its loop.
    _worker_backends(monkeypatch, ["iter one words"])
    tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
    payload.pop("backend")
    payload["work_branch"] = "improve/retry-branch"
    with make_api_client(cfg, session_mgr, tree) as client:
        resp = client.post("/api/internal/improve", json=payload, headers=OPERATOR)
        assert resp.status_code == 200, resp.text
        body = resp.json()
        child_id = body["child_session_id"]
        deadline = asyncio.get_event_loop().time() + 30
        state_status = None
        while asyncio.get_event_loop().time() < deadline:
            state = await load_loop_state(manager.id, body["loop_id"], cfg)
            if state is not None and state.status != "running":
                state_status = state.status
                break
            await asyncio.sleep(0.1)
        assert state_status is not None, "the retried loop never finished"
        assert child_id
        assert not _active_loop_path(manager.id, cfg).exists()


@pytest.mark.asyncio
async def test_improve_without_authorization_is_forbidden_not_a_server_error(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
    """No take-off anywhere in the chain: 403 with the gate's reason, never a
    500, and nothing reserved."""
    from src.core.improve_command import _active_loop_path, _loops_dir, find_running_loop
    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
    from src.core.models import TaskSpec
    manager = await tree.create_task(
        request_id="root", task_parent_id=None, profile="manager",
        task=TaskSpec(goal="pm"), name="PM", backend=None, caller="operator")
    stub_credentials(monkeypatch, {"charliebot": {"access_key": "op-secret"}})
    monkeypatch.setenv("CHARLIEBOT_HOME", str(cfg.charliebot_home))
    payload = {
        "session_id": manager.id,
        "goal": "## Goal\n\nimprove the thing\n",
        "iterations": 1,
        "repo_path": str(repo),
        "base_branch": "main",
    }
    with make_api_client(cfg, session_mgr, tree) as client:
        resp = client.post("/api/internal/improve", json=payload, headers=OPERATOR)
    assert resp.status_code == 403, resp.text
    assert "take off" in str(resp.json()["detail"]).lower() or "authorization" in str(resp.json()["detail"]).lower()
    assert not _active_loop_path(manager.id, cfg).exists()
    assert await find_running_loop(manager.id, cfg) is None
    loops_dir = _loops_dir(manager.id, cfg)
    assert not loops_dir.is_dir() or list(loops_dir.glob("*")) == []


# ---------------------------------------------------------------------------
# Withheld launches settle: no hung waiter, released lock, honest report
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_withheld_iteration_launch_settles_the_loop_without_hanging(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
    """A launch precondition that fails in the registration-to-launch interval
    (here: the loop's child paused) must never leave the controller polling
    forever: the loop settles blocked with the actual reason, the active lock
    is released, the queued iteration Run stays as the retained pending
    request, and the ONE blocked report lands on the manager."""
    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
    from src.core.control_events import stable_run_id
    from src.core.improve_command import (
        _active_loop_path,
        reserve_loop_state,
    )
    from src.core.improve_sequence import create_improve_child, run_improve_sequence
    from src.core.models import TaskSpec
    manager = await tree.create_task(
        request_id="root", task_parent_id=None, profile="manager",
        task=TaskSpec(goal="pm"), name="PM", backend=None, caller="operator")
    await tree.dispatch.admit_input(
        manager.id, event_type=ET.USER, content="Take off. Improve the thing.", actor="user")
    install_backends(monkeypatch, [], "src.agents.worker.build_backend")
    tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)

    state = await reserve_loop_state(
        manager.id, "improve the thing", "improve/withheld", str(repo), cfg,
        base_branch="main", resolved_backend="fake", resolved_model=None)
    child = await create_improve_child(
        tree, manager.id, state.loop_id, "improve the thing",
        repo_path=str(repo), base_branch="main")
    # The launch precondition fails before any process may start: the child
    # (the iteration Runs' node) is paused.
    from src.core.models import PatchSessionTaskRequest
    from src.core.run_token import CallerIdentity
    await tree.patch_task(
        child.id, PatchSessionTaskRequest(automation_paused=True), caller=CallerIdentity(kind="operator"))

    controller = asyncio.create_task(run_improve_sequence(
        manager.id, cfg, tree, loop_id=state.loop_id, iterations=2, child_id=child.id,
        goal="improve the thing"))
    await asyncio.wait_for(controller, 30)

    from src.core.improve_command import load_loop_state
    settled = await load_loop_state(manager.id, state.loop_id, cfg)
    assert settled is not None and settled.status == "blocked"
    assert not _active_loop_path(manager.id, cfg).exists(), (
        "a settled loop must release its active lock")
    # The queued iteration Run stays as the retained pending request (no
    # terminal fact, no side effects, no automatic retry).
    runs = tree.runs.list_run_records_sync(child.id)
    assert len(runs) == 1
    iteration_run_id = stable_run_id(child.id, f"improve:{state.loop_id}:iter:1")
    assert runs[0].id == iteration_run_id
    assert runs[0].pid is None
    assert tree.runs.terminal_outcome(tree.runs.load_events_sync(child.id), iteration_run_id) is None
    # The actual reason is delivered: one blocked report on the manager.
    reports = [e for e in tree.events.load_events(manager.id) if e.get("type") == ET.CHILD_REPORT]
    assert len(reports) == 1
    assert reports[0]["outcome"] == "blocked"
    assert "paused" in str(reports[0]["summary"])
    # The released lock allows an explicit restart (the existing resume policy).
    next_state = await reserve_loop_state(
        manager.id, "improve the thing", "improve/withheld-2", str(repo), cfg,
        base_branch="main", resolved_backend="fake", resolved_model=None)
    assert next_state.loop_id != state.loop_id


@pytest.mark.asyncio
async def test_stopped_queued_iteration_never_launches_and_settles(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
    """A durable stop request landing on a queued iteration is a settle fact:
    the launch refuses, the controller settles blocked, and the run keeps its
    stop request (never launched behind it)."""
    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
    from src.core.improve_command import reserve_loop_state
    from src.core.improve_sequence import create_improve_child, run_improve_sequence
    from src.core.models import TaskSpec
    manager = await tree.create_task(
        request_id="root", task_parent_id=None, profile="manager",
        task=TaskSpec(goal="pm"), name="PM", backend=None, caller="operator")
    await tree.dispatch.admit_input(
        manager.id, event_type=ET.USER, content="Take off. Improve the thing.", actor="user")
    install_backends(monkeypatch, [], "src.agents.worker.build_backend")
    tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)

    state = await reserve_loop_state(
        manager.id, "improve the thing", "improve/stopped", str(repo), cfg,
        base_branch="main", resolved_backend="fake", resolved_model=None)
    child = await create_improve_child(
        tree, manager.id, state.loop_id, "improve the thing",
        repo_path=str(repo), base_branch="main")
    from src.core.control_events import stable_run_id
    from src.core.models import RunRecord
    iteration_run_id = stable_run_id(child.id, f"improve:{state.loop_id}:iter:1")
    # Register the run first so the stop request has a record to bind to.
    await tree.runs.register_run(
        RunRecord(id=iteration_run_id, session_id=child.id, kind="iteration",
                  backend="fake", repo_path=str(repo), base_branch="main",
                  branch_name="improve/stopped",
                  worktree_path=str(Path(cfg.paths.worktree_dir) / "improve-stopped")))
    await tree.runs.request_stop(child.id, iteration_run_id, "stop before launch")

    controller = asyncio.create_task(run_improve_sequence(
        manager.id, cfg, tree, loop_id=state.loop_id, iterations=2, child_id=child.id,
        goal="improve the thing"))
    await asyncio.wait_for(controller, 30)

    from src.core.improve_command import load_loop_state
    settled = await load_loop_state(manager.id, state.loop_id, cfg)
    assert settled is not None and settled.status == "blocked"
    reports = [e for e in tree.events.load_events(manager.id) if e.get("type") == ET.CHILD_REPORT]
    assert len(reports) == 1 and reports[0]["outcome"] == "blocked"
    assert "stop" in str(reports[0]["summary"]).lower()


@pytest.mark.asyncio
async def test_launch_and_settle_follows_a_live_process_without_relaunching(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
    """The shared observation never duplicates a live process: a Run that
    already carries a pid is followed to its durable fact (however long it
    takes), and the settlement outcome is that run's own result — no second
    build, no kill for running long."""
    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
    from src.core.models import RunRecord, TaskSpec
    tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
    builds = install_backends(monkeypatch, [], "src.agents.worker.build_backend")
    manager = await tree.create_task(
        request_id="root", task_parent_id=None, profile="manager",
        task=TaskSpec(goal="pm"), name="PM", backend=None, caller="operator")
    await tree.dispatch.admit_input(
        manager.id, event_type=ET.USER, content="Take off. Go.", actor="user")
    child = await tree.create_task(
        request_id="leaf", task_parent_id=manager.id, profile="worker",
        task=TaskSpec(goal="work"), name="W", backend="fake", caller="operator")
    run = await tree.runs.register_run(
        RunRecord(id="live-run", session_id=child.id, kind="work", backend="fake",
                  repo_path=str(repo), base_branch="main", branch_name="live",
                  worktree_path=str(Path(cfg.paths.worktree_dir) / "live")))
    # A live process identity with no terminal fact yet (as a recovered or
    # replayed registration sees it).
    await tree.runs.record_launch(child.id, run.id, pid=424001, pid_start="1-424001")
    settled = asyncio.create_task(
        tree.dispatch.executor.launch_and_settle(child.id, run.id))
    # The process's own follow lands the durable fact (whenever it lands, the
    # follower returns it — the pid alone decided not to relaunch).
    await tree.runs.record_finish(child.id, run.id, outcome="success")
    observation = await asyncio.wait_for(settled, 10)
    assert observation.withheld is None
    assert observation.outcome == "success"
    assert builds == []  # followed, never relaunched
