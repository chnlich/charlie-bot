"""Execution-adapter tests: v2 Runs bound to the existing master/worker harnesses.

Covers the required behavior wiring: durable dispatch to actual execution (one
Run, exact batch, launched identity persisted before its credential works),
delegate as task/run with stable replay identity, queued retry launches,
failure requiring explicit retry, first-terminal-fact-wins, worker work/review
delivery through the same task, and real landing verification.
"""

from __future__ import annotations

import asyncio
import errno
import json
import os
import subprocess
import time
import uuid
from collections.abc import AsyncIterator, Awaitable, Callable
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest
from conftest import (
    BUILD_BACKEND_PATCH_TARGET,
    FABLE_MODEL,
    OPERATOR,
    POOLED_FABLE_ID,
    WORKER_BUILD_BACKEND_PATCH_TARGET,
    ScriptedRelayBackend,
    _settle_parent,
    assistant_text_event,
    backend_option,
    create_task,
    init_repo_with_origin,
    install_scripted_backends,
    make_transcript,
    patch_instructions_content,
    pool_cfg,
    rate_limit_event,
    run_git,
    stub_credentials,
)
from fastapi import FastAPI
from fastapi.testclient import TestClient

import src.core.task_execution as task_execution_module
from src.core import claude_accounts, claude_relay
from src.core import event_types as ET
from src.core.models import BackendOption, PatchSessionTaskRequest, RunRecord, TaskSpec
from src.core.runs import RAW_LOG_NAME
from src.core.sessions import (
    CONTEXT_RESET_INSTRUCTION,
    HISTORY_LOCATION_NOTE,
    SessionManager,
)
from src.core.task_sessions import TaskTreeManager

# The internal-API auth headers carrying the access key stub_credentials seeds: tests
# pass it as headers=. It is not a caller identity; conftest's OPERATOR (CallerIdentity)
# is what caller= takes.
OP_HEADERS = {"Authorization": "Bearer op-secret"}


def seed_signing_home(home: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """Make the synthetic home the one spawned children sign against: seed its
    access key and pin CHARLIEBOT_HOME at it, so every child environment signs
    its run token against the synthetic home (never operator credentials)."""
    import src.core.config as core_config
    core_config._credentials_cache.seed(
        core_config.Credentials(
            path=home / "credentials.yaml",
            sections={"charliebot": {"access_key": "task-exec-test-key"}}))
    monkeypatch.setenv("CHARLIEBOT_HOME", str(home))


def build_spawning_env(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, *, options: list,
                       preference: list[str] | None = None):
    """(cfg, SessionManager, TaskTreeManager) over one synthetic home under tmp_path:
    the given backend options, worktree_dir inside the home, and the seeded access
    key every child environment signs against (seed_signing_home)."""
    from src.core.config import CharlieBotConfig
    home = tmp_path / "charliebot-home"
    backends: dict = {"options": options}
    if preference is not None:
        backends["preference"] = preference
    cfg = CharlieBotConfig(
        charliebot_home=home,
        backends=backends,
        paths={"worktree_dir": str(home / "worktrees")})
    seed_signing_home(home, monkeypatch)
    session_mgr = SessionManager(cfg)
    return cfg, session_mgr, TaskTreeManager(cfg, session_mgr)


def build_env(tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
              backend_ids: list[str] | None = None):
    """One fake backend registered, so task creation's default resolution works.

    ``backend_ids`` names every configured option (default: the single "fake");
    tests that switch a session's backend pin build a config with more than one.
    """
    ids = backend_ids if backend_ids is not None else ["fake"]
    return build_spawning_env(
        tmp_path,
        monkeypatch,
        options=[backend_option(id=i, label=i, type="codex", model="fake-model") for i in ids],
        preference=ids[:1])


class SpawningScriptedBackend:
    """Backend double that fires the on_spawn callback, records its launch env,
    and yields a scripted event list ending in a result event. A *post_events*
    hook is awaited after that stream, before run() returns: it is how a test
    acts on the world between one build's events and the next build's prompt."""

    def __init__(self, events: list[dict], exit_code: int = 0, stderr_text: str = "",
                 pre_run: Callable[[], None] | None = None,
                 gate: Callable[[], object] | None = None,
                 post_events: Callable[[], Awaitable[None]] | None = None) -> None:
        self._events = events
        self.exit_code = exit_code
        self.stderr_text = stderr_text
        self._pre_run = pre_run
        self.gate = gate
        self.post_events = post_events
        self.pid_start = "1-424000"
        self.terminated = False
        self.hang_diagnostics = None
        self.prompt: str | None = None
        self.env: dict | None = None
        self.cwd: str | None = None
        self._on_spawn = None
        self._pid = 424000
        self.cgroup_exit_report = lambda: None

    def set_on_spawn(self, on_spawn) -> None:
        self._on_spawn = on_spawn

    async def terminate(self) -> None:
        self.terminated = True

    def detach(self) -> None:
        pass

    async def run(self, prompt: str, cwd: str, env: dict,
                  uploaded_files: list[dict] | None = None) -> AsyncIterator[dict]:
        self.prompt = prompt
        self.cwd = cwd
        self.env = dict(env)
        self._pid += 1
        if self._pre_run is not None:
            self._pre_run()
        if self.gate is not None:
            await asyncio.wait_for(self.gate(), timeout=15)
        if self._on_spawn is not None:
            await self._on_spawn(self._pid)
        for event in self._events:
            if self.terminated:
                return
            yield event
        if self.post_events is not None:
            await self.post_events()


def result_event(text: str) -> dict:
    """A result event carrying real usage and text (the zero-output guard reads both)."""
    from src.agents.backends import base as backend_base
    event = backend_base.make_result_event(input_tokens=10, output_tokens=5)
    event["result"] = text
    return event


def install_backends(monkeypatch: pytest.MonkeyPatch, backends: list, target: str) -> list[dict]:
    """Serve *backends* one build at a time, wiring each build's on_spawn into the double."""
    builds: list[dict] = []
    queue = list(backends)

    def fake_build(option: BackendOption, cfg, **kwargs):
        backend = queue.pop(0)
        on_spawn = kwargs.get("on_spawn")
        if on_spawn is not None:
            backend.set_on_spawn(on_spawn)
        builds.append({"option": option, "kwargs": kwargs, "backend": backend})
        return backend

    monkeypatch.setattr(target, fake_build)
    return builds


def make_pm_build(text: str, pm_builds: list | None = None):
    """The parent-manager turn's build function for BUILD_BACKEND_PATCH_TARGET:
    one scripted double per registry build, wired for on_spawn the way
    install_backends wires worker builds. ``pm_builds`` collects the built
    doubles for tests that assert the parent turn actually built one."""

    def pm_build(option, cfg_, **kwargs):
        b = SpawningScriptedBackend([result_event(text)])
        on_spawn = kwargs.get("on_spawn")
        if on_spawn is not None:
            b.set_on_spawn(on_spawn)
        if pm_builds is not None:
            pm_builds.append(b)
        return b

    return pm_build


def make_api_client(cfg, session_mgr, task_mgr) -> TestClient:
    from src.api import internal as internal_api
    from src.api import sessions as sessions_api
    from src.api import threads as threads_api
    from src.api.deps import get_config, get_config_on_loop, get_run_store, get_session_manager, get_task_manager

    app = FastAPI()
    app.include_router(sessions_api.router, prefix="/api/sessions")
    app.include_router(threads_api.router, prefix="/api/threads")
    app.include_router(internal_api.router, prefix="/api/internal")
    app.dependency_overrides[get_config] = lambda: cfg
    app.dependency_overrides[get_config_on_loop] = lambda: cfg
    app.dependency_overrides[get_session_manager] = lambda: session_mgr
    app.dependency_overrides[get_task_manager] = lambda: task_mgr
    app.dependency_overrides[get_run_store] = lambda: task_mgr.runs
    return TestClient(app)


async def wait_for_terminal_run(tree: TaskTreeManager, session_id: str, run_id: str,
                                timeout: float = 15.0) -> tuple[RunRecord, str]:
    """Poll one Run until its end record lands (the launch is fire-and-forget).

    The terminal fact and the metadata mirror land as two writes under one
    lock; a poll that reads the record between them returns a stale record
    whose ``ended_at`` is still empty, so the terminal outcome alone is not
    the landing's edge — wait for the mirror's ``ended_at`` too.
    """
    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        run = await tree.runs.get_run(session_id, run_id)
        assert run is not None, f"run {run_id} vanished"
        events = tree.runs.load_events_sync(session_id)
        outcome = tree.runs.terminal_outcome(events, run_id)
        if outcome is not None and run.ended_at is not None:
            return run, str(outcome)
        await asyncio.sleep(0.05)
    pytest.fail(f"run {run_id} never reached a terminal fact within {timeout}s")


def work_run_worktree(tree: TaskTreeManager, worker_id: str) -> Path:
    """The one work-kind run's worktree path (asserts exactly one work run)."""
    work = [r for r in tree.runs.list_run_records_sync(worker_id) if r.kind == "work"]
    assert len(work) == 1 and work[0].worktree_path
    return Path(work[0].worktree_path)


def implement_marker_commit(tree: TaskTreeManager, worker_id: str) -> None:
    """The implement backend's side effect: commit marker.txt on the work branch."""
    wt = work_run_worktree(tree, worker_id)
    (wt / "marker.txt").write_text("implemented\n")
    run_git(wt, "add", "-A")
    run_git(wt, "-c", "user.email=t@example.com", "-c", "user.name=t", "commit", "-q", "-m", "implement marker")


# ---------------------------------------------------------------------------
# Manager turns: durable dispatch to actual execution
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_manager_turn_persists_run_identity_and_acknowledges_batch(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    cfg, _session_mgr, tree, manager = await _wired_root_manager(tmp_path, monkeypatch)
    backend = SpawningScriptedBackend([result_event("SMOKE reply")])
    builds = install_backends(
        monkeypatch, [backend], BUILD_BACKEND_PATCH_TARGET)
    patch_instructions_content(monkeypatch)

    admitted = await tree.dispatch.admit_input(
        manager.id, event_type=ET.USER, content="Take off. Reply with the phrase.",
        actor="user")
    decision = await tree.dispatch.dispatch_pending(manager.id)
    assert decision["launch"] is True
    run_id = decision["run_id"]
    assert run_id is not None

    run, outcome = await wait_for_terminal_run(tree, manager.id, run_id)
    from conftest import drain_session_consumer
    await drain_session_consumer(manager.id, timeout=5)

    runs = tree.runs.list_run_records_sync(manager.id)
    assert [r.id for r in runs] == [run_id]
    assert run.kind == "manager_turn"
    assert run.pid == 424001 and run.pid_start == "1-424000"
    assert run.native_session_id is None or isinstance(run.native_session_id, str)
    assert run.model == "fake-model"
    # The ref names the Run's own transport dir (the backend double writes no
    # raw file; the live smoke asserts the real file exists).
    assert run.raw_log_ref == str(
        tree.runs.run_dir(manager.id, run_id) / "agent.raw.ndjson")
    assert run.input_event_ids == [str(admitted["id"])]
    assert outcome == "success"
    # The exact claimed batch is acknowledged; no second synthetic USER copy
    # was persisted and no master_run mirror exists for the v2 turn.
    assert tree.dispatch.pending_inputs(manager.id) == []
    events = tree.events.load_events(manager.id)
    assert [e["type"] for e in events if e["type"] == ET.USER] == ["user"]
    assert not (cfg.sessions_dir / manager.id / "data" / "master_runs").exists()
    # The launched process identity is the precondition of a run credential:
    # a queued run's token is rejected, a launched run's is accepted, and a
    # finished run's is not.
    from fastapi import HTTPException

    from src.api.deps import require_caller
    from src.core.run_token import RunTokenClaims
    queued_claims = RunTokenClaims(session_id=manager.id, run_id="run-queued", agent=manager.name or "m")
    await tree.runs.register_run(RunRecord(id="run-queued", session_id=manager.id, kind="manager_turn"))
    with pytest.raises(HTTPException, match="has not launched"):
        await require_caller(_bearer(queued_claims), tree.runs)
    launched_claims = RunTokenClaims(session_id=manager.id, run_id="run-launched", agent=manager.name or "m")
    await tree.runs.register_run(RunRecord(id="run-launched", session_id=manager.id, kind="manager_turn"))
    await tree.runs.record_launch(manager.id, "run-launched", pid=424900, pid_start="ps-900")
    identity = await require_caller(_bearer(launched_claims), tree.runs)
    assert identity.claims.run_id == "run-launched"
    finished_claims = RunTokenClaims(session_id=manager.id, run_id=run_id, agent=manager.name or "m")
    with pytest.raises(HTTPException, match="active run"):
        await require_caller(_bearer(finished_claims), tree.runs)
    assert len(builds) == 1
    assert builds[0]["kwargs"].get("on_spawn") is not None
    captured_env = builds[0]["backend"].env or {}
    assert captured_env.get("CHARLIEBOT_SESSION_ID") == manager.id
    assert captured_env.get("CHARLIEBOT_HOME") == str(cfg.charliebot_home)


def _bearer(claims):
    """A signed run-token request against the synthetic home's seeded key."""
    import src.core.config as core_config
    from src.core.run_token import sign_run_token
    key = str(core_config.get_credentials().get("charliebot", "access_key") or "")
    token = sign_run_token(claims, key)
    # Lowercase key: the real Header object is case-insensitive; the plain
    # dict double must match the exact key require_caller reads.
    request = type("R", (), {"headers": {"authorization": f"Bearer {token}"}})()
    return request


def _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch):
    from src.agents.master_cc_queue import streaming_manager
    from src.core.task_execution import TaskExecutionAdapter
    monkeypatch.setattr(streaming_manager, "broadcast", _async_noop)
    return TaskExecutionAdapter(cfg, session_mgr, tree)


async def _async_noop(*args, **kwargs) -> None:
    return None


async def _launch_manager_turn(cfg, session_mgr, tree, monkeypatch, manager, content) -> list:
    """The launch rig the implement and quick-edit tests share: silent-broadcast
    executor, the parent-manager build double, the instructions patch, the
    operator credentials, and the user takeoff message through admit_input.

    Ordering is load-bearing: the executor sits in place before admit_input
    dispatches, and the build double before the manager turn builds. Returns
    the pm_builds list for tests that assert the parent turn built one.
    """
    tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
    pm_builds = []
    monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, make_pm_build("manager turn", pm_builds))
    patch_instructions_content(monkeypatch)
    stub_credentials({"charliebot": {"access_key": "op-secret"}})
    # The work-run launch re-judges the nearest-user authorization gate; the
    # manager carries the real user takeoff message the delegation rode in on.
    await tree.dispatch.admit_input(manager.id, event_type=ET.USER, content=content, actor="user")
    return pm_builds


@pytest.mark.asyncio
async def test_first_message_on_empty_goal_task_dispatches_a_manager_turn(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """The one-click New Task product (empty goal, no acceptance, no refs) takes
    the user's first message through the normal durable input path: the message
    is admitted, one manager_turn Run claims exactly that batch, and the turn
    executes — no Goal-required obstacle anywhere in the dispatch."""
    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
    manager = await create_task(
        tree, parent=None, request_id="one-click-root", profile="manager",
        task=TaskSpec(goal="", acceptance=[], context_refs=[]), name=None)
    assert manager.task is not None and manager.task.goal == ""
    tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
    backend = SpawningScriptedBackend([result_event("SMOKE reply")])
    install_backends(monkeypatch, [backend], BUILD_BACKEND_PATCH_TARGET)
    patch_instructions_content(monkeypatch)

    admitted = await tree.dispatch.admit_input(
        manager.id, event_type=ET.USER, content="First message on a brand-new task.",
        actor="user")
    decision = await tree.dispatch.dispatch_pending(manager.id)
    assert decision["launch"] is True
    run_id = decision["run_id"]

    run, outcome = await wait_for_terminal_run(tree, manager.id, run_id)
    assert outcome == "success"
    assert run.kind == "manager_turn"
    assert run.input_event_ids == [str(admitted["id"])]
    assert tree.dispatch.pending_inputs(manager.id) == []
    events = tree.events.load_events(manager.id)
    user_events = [e for e in events if e["type"] == ET.USER]
    assert [e["content"] for e in user_events] == ["First message on a brand-new task."]


@pytest.mark.asyncio
async def test_concurrent_dispatch_starts_one_process(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _cfg, _session_mgr, tree, manager = await _wired_root_manager(tmp_path, monkeypatch)
    backend = SpawningScriptedBackend([result_event("one")])
    builds = install_backends(
        monkeypatch, [backend], BUILD_BACKEND_PATCH_TARGET)
    patch_instructions_content(monkeypatch)

    await tree.dispatch.admit_input(
        manager.id, event_type=ET.USER, content="Take off. First message.", actor="user")
    # Two dispatch calls race the same pending batch: the reservation serializes
    # them, so exactly one Run and one process exist.
    results = await asyncio.gather(
        tree.dispatch.dispatch_pending(manager.id),
        tree.dispatch.dispatch_pending(manager.id))
    launched = [d for d in results if d.get("launch")]
    assert len(launched) == 1
    run_id = launched[0]["run_id"]
    await wait_for_terminal_run(tree, manager.id, run_id)
    assert len(tree.runs.list_run_records_sync(manager.id)) == 1
    assert len(builds) == 1
    assert tree.dispatch.pending_inputs(manager.id) == []


# ---------------------------------------------------------------------------
# Serialized turns: inputs admitted during an active run dispatch on its finish
# ---------------------------------------------------------------------------


class _SpawnFirstBackend(SpawningScriptedBackend):
    """Records the spawn identity first, then holds the turn open until released."""

    async def run(self, prompt, cwd, env, uploaded_files=None):
        self.prompt = prompt
        self.cwd = cwd
        self.env = dict(env)
        self._pid += 1
        if self._on_spawn is not None:
            await self._on_spawn(self._pid)
        if self.gate is not None:
            await asyncio.wait_for(self.gate(), timeout=15)
        for event in self._events:
            if self.terminated:
                return
            yield event


@pytest.mark.asyncio
async def test_input_admitted_during_active_run_dispatches_after_its_finish(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """The later input is consumed by the next serialized turn without a second dispatch call."""
    _cfg, _session_mgr, tree, manager = await _wired_root_manager(tmp_path, monkeypatch)
    gate_release = asyncio.Event()
    first = _SpawnFirstBackend([result_event("first")], gate=gate_release.wait)
    second = _SpawnFirstBackend([result_event("second")])
    install_backends(
        monkeypatch, [first, second], BUILD_BACKEND_PATCH_TARGET)
    patch_instructions_content(monkeypatch)

    await tree.dispatch.admit_input(
        manager.id, event_type=ET.USER, content="Take off. First.", actor="user")
    decision = await tree.dispatch.dispatch_pending(manager.id)
    assert decision["launch"] is True
    run1 = decision["run_id"]

    deadline = asyncio.get_event_loop().time() + 10
    while asyncio.get_event_loop().time() < deadline:
        run = await tree.runs.get_run(manager.id, run1)
        if run is not None and run.pid is not None:
            break
        await asyncio.sleep(0.05)
    else:
        pytest.fail("the first turn never launched")

    # The later input is durably admitted but never launched under the active run.
    admitted_later = await tree.dispatch.admit_input(
        manager.id, event_type=ET.USER, content="Second message.", actor="user")
    decision_later = await tree.dispatch.dispatch_pending(manager.id)
    assert decision_later["launch"] is False

    gate_release.set()
    await wait_for_terminal_run(tree, manager.id, run1)

    # The turn's own finish dispatches the waiting input as the next serialized
    # turn: no second explicit dispatch call is needed and nothing is dropped.
    deadline = asyncio.get_event_loop().time() + 15
    run2_id = None
    while asyncio.get_event_loop().time() < deadline:
        others = [r for r in tree.runs.list_run_records_sync(manager.id) if r.id != run1]
        if others:
            run2_id = others[0].id
            break
        await asyncio.sleep(0.1)
    assert run2_id is not None, "the later input was never dispatched after the turn finished"
    run2, outcome2 = await wait_for_terminal_run(tree, manager.id, run2_id)
    assert outcome2 == "success"
    assert run2.input_event_ids == [str(admitted_later["id"])]
    assert tree.dispatch.pending_inputs(manager.id) == []


# ---------------------------------------------------------------------------
# Delegate: one worker child task with its first Run, stable replay identity
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_delegate_creates_one_child_and_replays_are_stable(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:

    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
    manager = await create_task(tree, parent=None, request_id="root")
    monkeypatch.setenv("CHARLIEBOT_HOME", str(cfg.charliebot_home))
    stub_credentials({"charliebot": {"access_key": "op-secret"}})
    pm_builds = []
    monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, make_pm_build("manager turn", pm_builds))
    backend = SpawningScriptedBackend([result_event("phrase")])
    builds = install_backends(
        monkeypatch,
        [backend, SpawningScriptedBackend([result_event("phrase")]),
         SpawningScriptedBackend([result_event("phrase")])],
        WORKER_BUILD_BACKEND_PATCH_TARGET)
    tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
    repo, _origin = init_repo_with_origin(tmp_path / "delegate-work")

    # The nearest-user authorization gate re-judges at delegation: the manager
    # needs a real user message with the takeoff phrase.
    await tree.dispatch.admit_input(
        manager.id, event_type=ET.USER,
        content="Take off and delegate the phrase task.", actor="user")
    from src.api import internal as internal_api
    monkeypatch.setattr(internal_api, "get_config", lambda: cfg)
    with make_api_client(cfg, session_mgr, tree) as client:
        payload = {
            "session_id": manager.id,
            "description": "## Goal\n\nsay the phrase\n",
            "task_type": "quick-edit",
            "keep_worktree": False,
            "repo_path": str(repo),
            "base_branch": "main",
        }
        first = client.post("/api/internal/delegate", json=payload, headers=OP_HEADERS)
        assert first.status_code == 200, first.text
        body = first.json()
        child_id, run_id = body["session_id"], body["run_id"]
        assert body["parent_session_id"] == manager.id
        assert body["thread_id"] == run_id
        child_meta = await tree.load_meta(child_id)
        assert child_meta is not None and child_meta.profile == "worker"
        assert child_meta.task_parent_id == manager.id

        # The replayed request (same spec, derived stable id) returns the
        # original child and Run instead of a second process.
        replay = client.post("/api/internal/delegate", json=payload, headers=OP_HEADERS)
        assert replay.status_code == 200, replay.text
        assert replay.json()["session_id"] == child_id
        assert replay.json()["run_id"] == run_id

        # The same explicit request_id replays identically; a distinct explicit
        # id names a genuinely different operation (an intentional sibling).
        first_named = client.post("/api/internal/delegate",
                                  json=dict(payload, request_id="op-1"), headers=OP_HEADERS)
        assert first_named.json()["session_id"] != child_id
        replay_named = client.post("/api/internal/delegate",
                                   json=dict(payload, request_id="op-1"), headers=OP_HEADERS)
        assert replay_named.json()["session_id"] == first_named.json()["session_id"]
        sibling = client.post("/api/internal/delegate", json=dict(payload, request_id="op-2"),
                              headers=OP_HEADERS)
        assert sibling.json()["session_id"] != first_named.json()["session_id"]

        # The runs execute through the worker adapter while the API loop that
        # scheduled them is still alive. Every delivery follow-up — child close,
        # parent report dispatch, the parent's serialized turns — is scheduled
        # on that same loop, so it must outlive them all.
        deadline = asyncio.get_event_loop().time() + 30
        while asyncio.get_event_loop().time() < deadline:
            states = [tree.task_state(s) for s in (
                child_id, first_named.json()["session_id"], sibling.json()["session_id"])]
            if all(s == "completed" for s in states):
                break
            await asyncio.sleep(0.1)
        for cid in (child_id, first_named.json()["session_id"], sibling.json()["session_id"]):
            assert tree.task_state(cid) == "completed"

        # The completed delivery closed and auto-archived the worker; the
        # manager remains open and received the completed report.
        deadline = asyncio.get_event_loop().time() + 20
        archived = False
        while asyncio.get_event_loop().time() < deadline:
            if tree.task_state(child_id) == "completed" and tree.archived_of(
                    await tree._get_index(), await tree.load_meta(child_id)):
                archived = True
                break
            await asyncio.sleep(0.1)
        assert archived, "the worker task was not auto-archived after its delivered report"
        assert tree.task_state(manager.id) == "open"
        reports = [e for e in tree.events.load_events(manager.id)
                   if e["type"] == ET.CHILD_REPORT and e["child_session_id"] == child_id]
        assert reports and reports[-1]["outcome"] == "completed"

        # The parent-addressed compatibility alias (thread_id from the delegate
        # contract) resolves to the child's Run — not to a run on the parent.
        resolved = tree.aliases.resolve_thread(manager.id, run_id)
        assert resolved == {"session_id": child_id, "run_id": run_id}
        from src.api import deps
        monkeypatch.setattr(deps, "_task_manager", tree)
        row = client.get(f"/api/threads/{manager.id}/threads/{run_id}", headers=OP_HEADERS)
        assert row.status_code == 200, row.text
        assert row.json()["id"] == run_id
        assert row.json()["session_id"] == child_id

        # The legacy list route exposes the same Run as a compatibility row:
        # its real id, backend and finished status — no ThreadMetadata exists.
        listed = client.get(f"/api/threads/{child_id}/list", headers=OP_HEADERS)
        assert listed.status_code == 200, listed.text
        rows = listed.json()
        assert isinstance(rows, list)
        compat = [r for r in rows if r.get("id") == run_id]
        assert compat, f"the worker run did not surface in the legacy list: {listed.text[:400]}"
        assert compat[0]["backend"] == "fake" and compat[0]["status"] == "completed"
        legacy_dir = cfg.sessions_dir / child_id / "threads"
        assert not list(legacy_dir.iterdir()) if legacy_dir.is_dir() else True

        # Every delivered child report triggered the parent's next serialized
        # turn: all reports were consumed and acknowledged, the parent stays open.
        await _settle_parent(tree, manager, timeout=30.0, poll=0.2)
        assert pm_builds, "the delivered child reports never triggered a parent manager turn"
        assert tree.task_state(manager.id) == "open"

    # Three distinct operations, three builds: the replays did not spawn a
    # second process for their operation.
    assert len(builds) == 3
    captured_env = builds[0]["backend"].env or {}
    assert captured_env.get("CHARLIEBOT_SESSION_ID") == child_id
    assert captured_env.get("CHARLIEBOT_RUN_TOKEN")
    assert captured_env.get("CHARLIEBOT_HOME") == str(cfg.charliebot_home)
    worker_run = await tree.runs.get_run(child_id, run_id)
    assert worker_run is not None and worker_run.kind == "work"
    assert worker_run.backend == "fake"
    pm_events = tree.runs.load_events_sync(manager.id)
    for r in tree.runs.list_run_records_sync(manager.id):
        assert tree.runs.terminal_outcome(pm_events, r.id) == "success"


# ---------------------------------------------------------------------------
# Retry: queued runs, stops, and the explicit-retry gate
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_manager_retry_reruns_its_own_batch_and_stopped_retry_never_launches(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _cfg, _session_mgr, tree, manager = await _wired_root_manager(tmp_path, monkeypatch)
    patch_instructions_content(monkeypatch)

    # A manager round claims its batch and fails. The failed round counts as
    # handled whatever its outcome: its batch never reappears as pending, and
    # a new message dispatches a new round at once — no explicit retry gate.
    first_in = await tree.dispatch.admit_input(
        manager.id, event_type=ET.USER, content="Take off. First.", actor="user")
    await tree.runs.register_run(
        RunRecord(id="run-failed", session_id=manager.id, kind="manager_turn",
                  backend="fake", model="fake-model"))
    await tree.dispatch.claim_input_batch(manager.id, "run-failed")
    await tree.dispatch.finish_run(manager.id, "run-failed", outcome="failed", exit_code=1)
    assert tree.dispatch.pending_inputs(manager.id) == []
    admitted = await tree.dispatch.admit_input(
        manager.id, event_type=ET.USER, content="Take off. Next.", actor="user")
    backend = SpawningScriptedBackend([result_event("next round")])
    install_backends(monkeypatch, [backend], BUILD_BACKEND_PATCH_TARGET)
    decision = await tree.dispatch.dispatch_pending(manager.id)
    assert decision["launch"] is True
    next_run, _outcome = await wait_for_terminal_run(tree, manager.id, decision["run_id"])
    assert next_run.input_event_ids == [str(admitted["id"])]

    # The explicit retry of the failed manager round reruns that round's OWN
    # batch: the retry Run binds the original's input_event_ids, so it
    # launches with nothing pending and finishes with exactly that payload.
    retry = await tree.create_retry(manager.id, "retry-1", "run-failed")
    retry_run = await tree.runs.get_run(manager.id, retry["run_id"])
    assert retry_run is not None and retry_run.input_event_ids == [str(first_in["id"])]
    backend2 = SpawningScriptedBackend([result_event("retried")])
    install_backends(monkeypatch, [backend2], BUILD_BACKEND_PATCH_TARGET)
    decision = await tree.dispatch.dispatch_pending(manager.id)
    assert decision["launch"] is True and decision["run_id"] == retry["run_id"]
    retried, _retried_outcome = await wait_for_terminal_run(tree, manager.id, retry["run_id"])
    assert retried.input_event_ids == [str(first_in["id"])]
    finished = [e for e in tree.events.load_events(manager.id)
                if e["type"] == ET.RUN_FINISHED and e.get("run_id") == retry["run_id"]]
    assert finished and finished[0]["input_event_ids"] == [str(first_in["id"])]

    # A stop request on a queued retry keeps THAT retry from ever launching;
    # the pending message still dispatches as a fresh round of its own.
    third_in = await tree.dispatch.admit_input(
        manager.id, event_type=ET.USER, content="Take off. Again.", actor="user")
    retry2 = await tree.create_retry(manager.id, "retry-2", "run-failed")
    await tree.runs.request_stop(manager.id, retry2["run_id"], "stop-1")
    decision = await tree.dispatch.dispatch_pending(manager.id)
    assert decision["launch"] is True and decision["run_id"] != retry2["run_id"]
    fresh_run, _fresh_outcome = await wait_for_terminal_run(tree, manager.id, decision["run_id"])
    assert fresh_run.input_event_ids == [str(third_in["id"])]
    stopped_run = await tree.runs.get_run(manager.id, retry2["run_id"])
    assert stopped_run is not None and stopped_run.pid is None
    assert tree.runs.terminal_outcome(
        tree.runs.load_events_sync(manager.id), retry2["run_id"]) is None
    assert tree.dispatch.pending_inputs(manager.id) == []


@pytest.mark.asyncio
async def test_first_terminal_fact_wins_governs_followups(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _cfg, _session_mgr, tree = build_env(tmp_path, monkeypatch)
    root = await create_task(tree, parent=None, request_id="root")
    worker = await create_task(tree, parent=root.id, request_id="w", profile="worker")
    await tree.runs.register_run(RunRecord(id="run-w", session_id=worker.id, kind="work"))
    # Two concurrent finishers: the recorded failed fact stands even though the
    # losing finisher passed outcome="success", so the worker never closes.
    await asyncio.gather(
        tree.dispatch.finish_run(worker.id, "run-w", outcome="failed", exit_code=1),
        tree.dispatch.finish_run(worker.id, "run-w", outcome="success", exit_code=0),
    )
    assert tree.runs.terminal_outcome(
        tree.runs.load_events_sync(worker.id), "run-w") == "failed"
    assert tree.task_state(worker.id) == "open"
    assert [e for e in tree.events.load_events(root.id) if e["type"] == ET.CHILD_REPORT] == []


# ---------------------------------------------------------------------------
# Implement delivery: same-task review, real landing verification
# ---------------------------------------------------------------------------


@pytest.mark.integration
@pytest.mark.asyncio
async def test_implement_delivery_requires_review_and_real_landing(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
    repo, _origin = init_repo_with_origin(tmp_path)
    manager = await create_task(tree, parent=None, request_id="root")
    task_spec = {
        "goal": "## Goal\n\nadd a marker file\n",
        "repo_path": str(repo),
        "base_branch": "origin/main",
        "task_type": "implement",
        "keep_worktree": False,
    }
    worker = await create_task(tree, parent=manager.id, request_id="w", profile="worker",
                               task=_task_spec(tree, task_spec))
    pm_builds = await _launch_manager_turn(cfg, session_mgr, tree, monkeypatch, manager,
                                           "Take off and implement the marker file.")

    # The work run holds until the test has committed the implementation into
    # the isolated worktree (the fake backend writes no commits itself).
    work_committed: asyncio.Event = asyncio.Event()
    work_backend = SpawningScriptedBackend(
        [result_event("implemented")], gate=work_committed.wait)

    push_state = {"enabled": False}

    def reviewer_push() -> None:
        """The reviewer's landing work: rebase the branch onto the target and push."""
        if not push_state["enabled"]:
            return  # this review passes judgment without landing the branch
        record = tree.runs.read_run_sync(worker.id, "run-work")
        assert record is not None and record.worktree_path and record.branch_name
        work_wt = Path(record.worktree_path)
        for args in (("fetch", "-q", "origin"),
                     ("rebase", "-q", "origin/main"),
                     ("push", "-q", "origin", f"{record.branch_name}:main")):
            run_git(work_wt, *args)

    review_backend = SpawningScriptedBackend([result_event("review ok")], pre_run=reviewer_push)
    review_retry_backend = SpawningScriptedBackend([result_event("review ok again")], pre_run=reviewer_push)
    install_backends(
        monkeypatch, [work_backend, review_backend, review_retry_backend],
        WORKER_BUILD_BACKEND_PATCH_TARGET)

    record = RunRecord(id="run-work", session_id=worker.id, kind="work", backend="fake",
                       model="fake-model", repo_path=str(repo), base_branch="origin/main")
    await tree.runs.register_run(record, task_spec_text=task_spec["goal"])
    tree.dispatch.executor.launch(worker.id, "run-work")

    # The worker's implementation is its commit in the isolated worktree: wait
    # for the worktree, then land the work commit on the work branch.
    deadline = asyncio.get_event_loop().time() + 10
    while asyncio.get_event_loop().time() < deadline:
        run = await tree.runs.get_run(worker.id, "run-work")
        if run is not None and run.worktree_path:
            break
        await asyncio.sleep(0.05)
    else:
        pytest.fail("the work run never recorded its worktree")
    wt = Path(run.worktree_path)
    assert Path(run.worktree_path).is_dir()
    (wt / "marker.txt").write_text("implemented\n")
    run_git(wt, "add", "-A")
    run_git(wt, "-c", "user.email=t@example.com", "-c", "user.name=t", "commit", "-q", "-m", "implement marker")
    work_committed.set()

    deadline = asyncio.get_event_loop().time() + 10
    while asyncio.get_event_loop().time() < deadline:
        runs = {r.id: r for r in tree.runs.list_run_records_sync(worker.id)}
        review = [r for r in runs.values() if r.kind == "review"]
        if review and tree.runs.terminal_outcome(
                tree.runs.load_events_sync(worker.id), review[0].id) is not None:
            break
        await asyncio.sleep(0.1)
    else:
        pytest.fail("the review chain never produced a terminal fact")

    runs = {r.id: r for r in tree.runs.list_run_records_sync(worker.id)}
    review_runs = [r for r in runs.values() if r.kind == "review"]
    assert len(review_runs) == 1
    review_run = review_runs[0]
    # The review reuses the work Run's exact repo, branch and worktree.
    work_run = runs["run-work"]
    assert review_run.review_of_run_id == "run-work"
    assert review_run.repo_path == work_run.repo_path
    assert review_run.branch_name == work_run.branch_name
    assert review_run.worktree_path == work_run.worktree_path
    # Review-only success is not delivery: the work commit never landed on the
    # requested target, so the task stays open with a blocked report and the
    # worktree preserved.
    assert tree.task_state(worker.id) == "open"
    # The blocked report rides the delivery chain's own awaits (the landing
    # check shells out to git), so it can land after the review's terminal
    # fact is readable; wait for it the way the waits above wait for the
    # worktree and the terminal fact.
    reports: list[dict] = []
    deadline = asyncio.get_event_loop().time() + 10
    while asyncio.get_event_loop().time() < deadline:
        reports = [e for e in tree.events.load_events(manager.id) if e["type"] == ET.CHILD_REPORT]
        if reports and reports[-1]["outcome"] == "blocked":
            break
        await asyncio.sleep(0.05)
    assert reports and reports[-1]["outcome"] == "blocked"
    assert Path(work_run.worktree_path).exists()

    # The explicit authorized retry of the review — this time with the
    # reviewer's landing work enabled — executes through the same launch path,
    # and its landing completes the delivery.
    push_state["enabled"] = True
    retry = await tree.create_retry(worker.id, "review-retry-1", review_run.id)
    assert retry["run_id"]
    decision = await tree.dispatch.dispatch_pending(worker.id)
    assert decision["launch"] is True and decision["run_id"] == retry["run_id"]
    retry_review, retry_outcome = await wait_for_terminal_run(tree, worker.id, retry["run_id"])
    assert retry_outcome == "success"
    assert retry_review.review_of_run_id == "run-work"

    deadline = asyncio.get_event_loop().time() + 10
    while asyncio.get_event_loop().time() < deadline:
        if tree.task_state(worker.id) == "completed":
            break
        await asyncio.sleep(0.1)
    assert tree.task_state(worker.id) == "completed"
    reports = [e for e in tree.events.load_events(manager.id) if e["type"] == ET.CHILD_REPORT]
    assert reports[-1]["outcome"] == "completed"
    finished = [e for e in tree.events.load_events(worker.id) if e["type"] == ET.RUN_FINISHED]
    assert [e["outcome"] for e in finished][-1] == "success"

    # The delivered blocked and completed reports each triggered the parent's
    # next serialized turn, and the parent consumed them staying open.
    await _settle_parent(tree, manager, timeout=30.0, poll=0.2)
    assert pm_builds, "the delivered reports never triggered a parent manager turn"
    assert tree.task_state(manager.id) == "open"


# ---------------------------------------------------------------------------
# Bare-branch bases judge the local ref against origin (origin-tip starts, loud refusals)
# ---------------------------------------------------------------------------


def _advance_origin_from_second_clone(tmp_path: Path, origin: Path, filename: str) -> str:
    """Push one commit to the bare origin from a second clone; returns the new tip."""
    second = tmp_path / "second-clone"
    subprocess.run(["git", "clone", "-q", str(origin), str(second)], check=True)
    run_git(second, "config", "user.email", "t@example.com")
    run_git(second, "config", "user.name", "t")
    (second / filename).write_text("advance\n")
    run_git(second, "add", "-A")
    run_git(second, "commit", "-q", "-m", f"advance origin via {filename}")
    run_git(second, "push", "-q", "origin", "main")
    return run_git(second, "rev-parse", "refs/heads/main")


@pytest.mark.integration
@pytest.mark.asyncio
async def test_bare_branch_base_behind_starts_from_origin_tip(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """The bare-branch base form when the shared checkout's local branch is only
    behind origin (the 2026-09-26 incident shape): the Run launches anyway, its
    work branch starts at the origin tip, and the local main ref is untouched."""
    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
    repo, origin = init_repo_with_origin(tmp_path)
    manager = await create_task(tree, parent=None, request_id="root")
    task_spec = {
        "goal": "## Goal\n\nadd a marker file\n",
        "repo_path": str(repo),
        "base_branch": "main",  # the bare form: local main is judged against origin/main
        "task_type": "implement",
        "keep_worktree": False,
    }
    worker = await create_task(tree, parent=manager.id, request_id="w", profile="worker",
                               task=_task_spec(tree, task_spec))
    await _launch_manager_turn(cfg, session_mgr, tree, monkeypatch, manager,
                               "Take off and implement the marker file.")

    # Origin gains a commit from a second clone while the fixture repo's local
    # main stays put: local main is strictly behind origin/main.
    origin_tip = _advance_origin_from_second_clone(tmp_path, origin, "ahead.txt")
    local_main = run_git(repo, "rev-parse", "main")
    assert local_main != origin_tip

    work_backend = SpawningScriptedBackend([result_event("implemented")])
    review_backend = SpawningScriptedBackend([result_event("review ok")])
    install_backends(monkeypatch, [work_backend, review_backend], WORKER_BUILD_BACKEND_PATCH_TARGET)

    record = RunRecord(id="run-work", session_id=worker.id, kind="work", backend="fake",
                       model="fake-model", repo_path=str(repo), base_branch="main")
    await tree.runs.register_run(record, task_spec_text=task_spec["goal"])
    tree.dispatch.executor.launch(worker.id, "run-work")

    work_run, outcome = await wait_for_terminal_run(tree, worker.id, "run-work")
    assert outcome == "success"
    assert work_run.worktree_path
    work_wt = Path(work_run.worktree_path)
    # The work branch started at the new origin tip, never at the stale local main.
    assert run_git(work_wt, "rev-parse", "HEAD") == origin_tip
    assert run_git(repo, "rev-parse", "main") == local_main

    # The header the worker page projects for the launched Run: plain success,
    # both refs pointing at the files the launch produced.
    from src.core import worker_transcript
    entry = worker_transcript.load_worker_transcript(tree, worker.id)
    work_header = next(m for m in entry.projection.committed
                       if m.get("kind") == ET.RUN_HEADER and m.get("run_id") == "run-work")
    assert work_header["state"] == "success"
    assert work_header["launched"] is True and work_header["launch_failed"] is False
    assert Path(work_header["task_spec_ref"]).is_file()
    assert Path(work_header["launch_prompt_ref"]).is_file()

    # The chain settles through the real delivery path: the work branch adds
    # nothing beyond its base, so the review lands it and the task completes.
    reports: list[dict] = []
    deadline = asyncio.get_event_loop().time() + 15
    while asyncio.get_event_loop().time() < deadline:
        reports = [e for e in tree.events.load_events(manager.id) if e["type"] == ET.CHILD_REPORT]
        if reports and reports[-1]["outcome"] == "completed":
            break
        await asyncio.sleep(0.05)
    assert reports and reports[-1]["outcome"] == "completed"
    await _settle_parent(tree, manager, timeout=30.0, poll=0.2)


@pytest.mark.asyncio
async def test_bare_branch_base_with_unpushed_local_commit_launch_fails(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """The case the bare-branch base check still refuses: the local branch holds
    commits absent from origin. The real launch path lands the durable failure,
    and the worker page's header reads "launch failed" with the error text
    exactly once, a recorded task spec, and no launch prompt."""
    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
    repo, _origin = init_repo_with_origin(tmp_path)
    manager = await create_task(tree, parent=None, request_id="root")
    task_spec = {
        "goal": "## Goal\n\nadd a marker file\n",
        "repo_path": str(repo),
        "base_branch": "main",
        "task_type": "implement",
        "keep_worktree": False,
    }
    worker = await create_task(tree, parent=manager.id, request_id="w", profile="worker",
                               task=_task_spec(tree, task_spec))
    await _launch_manager_turn(cfg, session_mgr, tree, monkeypatch, manager,
                               "Take off and implement the marker file.")

    # One unpushed commit on the fixture repo's local main: origin does not
    # have it, so the base check fails closed.
    (repo / "local_only.txt").write_text("local work\n")
    run_git(repo, "add", "-A")
    run_git(repo, "-c", "user.email=t@example.com", "-c", "user.name=t", "commit", "-q", "-m", "unpushed local commit")
    local_tip = run_git(repo, "rev-parse", "main")

    work_backend = SpawningScriptedBackend([result_event("never runs")])
    install_backends(monkeypatch, [work_backend], WORKER_BUILD_BACKEND_PATCH_TARGET)

    record = RunRecord(id="run-work", session_id=worker.id, kind="work", backend="fake",
                       model="fake-model", repo_path=str(repo), base_branch="main")
    await tree.runs.register_run(record, task_spec_text=task_spec["goal"])
    tree.dispatch.executor.launch(worker.id, "run-work")

    work_run, outcome = await wait_for_terminal_run(tree, worker.id, "run-work")
    assert outcome == "failed"
    # The real launch-failure path: the Run never spawned (no started_at, the
    # backend double never ran), and the durable evidence carries the error.
    assert work_run.started_at is None
    assert work_backend.prompt is None
    error_text = _read_error_event(tree.runs.run_dir(worker.id, "run-work") / "events.jsonl")
    assert "BaseBranchResolutionError" in error_text
    assert "differs from origin/main" in error_text
    assert local_tip[:12] in error_text
    assert "fast-forward" not in error_text

    from src.core import worker_transcript
    entry = worker_transcript.load_worker_transcript(tree, worker.id)
    messages = entry.projection.committed
    header = messages[0]
    assert header["kind"] == ET.RUN_HEADER
    assert header["state"] == "failed"
    assert header["content"].endswith("launch failed")
    assert header["launched"] is False and header["launch_failed"] is True
    assert header["error"] == error_text
    # The error text appears exactly once across the projection: only in the
    # header's error field, never as an ordinary row.
    assert [m for m in messages if error_text in str(m.get("content") or "")] == []
    assert [m for m in messages if m.get("error") == error_text] == [header]
    assert header["task_spec_ref"] != "" and Path(header["task_spec_ref"]).is_file()
    assert header["launch_prompt_ref"] == ""  # never assembled: no process started

    # The failed launch reported to the parent through the real after-run path.
    report = await _wait_for_parent_report(tree, manager.id)
    assert report.get("outcome") == "failed"
    assert "differs from origin/main" in str(report.get("summary"))


# ---------------------------------------------------------------------------
# Repo-less work Runs: the Run directory is the working directory
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_repo_less_implement_delivers_after_review_passes(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
    manager = await create_task(tree, parent=None, request_id="root")
    task_spec = {
        "goal": "## Goal\n\nrefresh the host lint config\n\n## Acceptance Tests\n- config parses\n",
        "task_type": "implement",
    }
    worker = await create_task(tree, parent=manager.id, request_id="w", profile="worker",
                               task=_task_spec(tree, task_spec))
    pm_builds = await _launch_manager_turn(cfg, session_mgr, tree, monkeypatch, manager,
                                           "Take off and refresh the host lint config.")

    report = ("Created: /tmp/lint/ruff.toml\n"
              "Modified: /tmp/lint/setup.cfg\n"
              "Acceptance tests: config parses — pass")
    work_backend = SpawningScriptedBackend([result_event(report)])
    review_backend = SpawningScriptedBackend([result_event("review ok")])
    install_backends(
        monkeypatch, [work_backend, review_backend], WORKER_BUILD_BACKEND_PATCH_TARGET)

    record = RunRecord(id="run-work", session_id=worker.id, kind="work", backend="fake",
                       model="fake-model")
    await tree.runs.register_run(record, task_spec_text=task_spec["goal"])
    tree.dispatch.executor.launch(worker.id, "run-work")

    work_run, outcome = await wait_for_terminal_run(tree, worker.id, "run-work")
    assert outcome == "success"
    # Repo-less: the Run directory is the working directory; no worktree or
    # branch is ever recorded.
    assert work_run.repo_path is None and work_run.worktree_path is None
    assert work_run.branch_name is None
    assert work_backend.cwd == str(tree.runs.run_dir(worker.id, "run-work"))

    # The implement work Run's review spawns without a repo and reads the
    # spec-plus-paths context, not a diff.
    deadline = asyncio.get_event_loop().time() + 10
    while asyncio.get_event_loop().time() < deadline:
        runs = {r.id: r for r in tree.runs.list_run_records_sync(worker.id)}
        review = [r for r in runs.values() if r.kind == "review"]
        if review and tree.runs.terminal_outcome(
                tree.runs.load_events_sync(worker.id), review[0].id) is not None:
            break
        await asyncio.sleep(0.1)
    else:
        pytest.fail("the repo-less review never reached a terminal fact")
    review_run = review[0]
    assert review_run.review_of_run_id == "run-work"
    assert review_run.repo_path is None and review_run.worktree_path is None
    prompt = review_backend.prompt
    assert "no diff to read" in prompt
    assert "nothing to merge or push" in prompt
    assert "## Acceptance Tests" in prompt
    assert "Created: /tmp/lint/ruff.toml" in prompt

    # The reviewer's verdict is the delivery: the task closes and reports
    # completed to the parent with no landing step.
    deadline = asyncio.get_event_loop().time() + 10
    while asyncio.get_event_loop().time() < deadline:
        if tree.task_state(worker.id) == "completed":
            break
        await asyncio.sleep(0.1)
    else:
        pytest.fail("the repo-less implement task never closed after its review passed")
    # The close and the report land in separate awaits of the delivery chain;
    # a loaded runner's poll can see the closed task before the report
    # append, so the report gets the same bounded wait the review did.
    deadline = asyncio.get_event_loop().time() + 10
    while asyncio.get_event_loop().time() < deadline:
        reports = [e for e in tree.events.load_events(manager.id) if e["type"] == ET.CHILD_REPORT]
        if reports:
            break
        await asyncio.sleep(0.1)
    else:
        pytest.fail("the repo-less implement task never reported to its parent")
    assert reports[-1]["outcome"] == "completed"

    await _settle_parent(tree, manager, timeout=30.0, poll=0.2)
    assert pm_builds, "the delivered report never triggered a parent manager turn"
    assert tree.task_state(manager.id) == "open"


@pytest.mark.asyncio
async def test_repo_less_implement_review_failure_takes_the_failure_report(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
    manager = await create_task(tree, parent=None, request_id="root")
    task_spec = {"goal": "fix the host script", "task_type": "implement"}
    worker = await create_task(tree, parent=manager.id, request_id="w", profile="worker",
                               task=_task_spec(tree, task_spec))
    tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
    monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, make_pm_build("manager turn"))
    patch_instructions_content(monkeypatch)
    stub_credentials({"charliebot": {"access_key": "op-secret"}})
    await tree.dispatch.admit_input(
        manager.id, event_type=ET.USER, content="Take off and fix it.", actor="user")

    work_backend = SpawningScriptedBackend([result_event("done; modified /tmp/x.sh")])
    # The single configured reviewer fails (no result event: the durable
    # outcome is failed); the retry policy exhausts and the existing
    # blocked-report path reports to the parent.
    review_backend = SpawningScriptedBackend([])
    install_backends(
        monkeypatch, [work_backend, review_backend], WORKER_BUILD_BACKEND_PATCH_TARGET)

    record = RunRecord(id="run-work", session_id=worker.id, kind="work", backend="fake",
                       model="fake-model")
    await tree.runs.register_run(record, task_spec_text=task_spec["goal"])
    tree.dispatch.executor.launch(worker.id, "run-work")

    await wait_for_terminal_run(tree, worker.id, "run-work")
    deadline = asyncio.get_event_loop().time() + 15
    while asyncio.get_event_loop().time() < deadline:
        reports = [e for e in tree.events.load_events(manager.id) if e["type"] == ET.CHILD_REPORT]
        if reports and reports[-1]["outcome"] == "blocked":
            break
        await asyncio.sleep(0.1)
    else:
        pytest.fail("the failed repo-less review never reported blocked to the parent")
    assert "review of work run run-work failed on every configured reviewer backend" in reports[-1]["summary"]
    assert tree.task_state(worker.id) == "open"


@pytest.mark.asyncio
async def test_repo_less_quick_edit_closes_without_review(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
    manager = await create_task(tree, parent=None, request_id="root")
    task_spec = {"goal": "bump the host cron schedule line", "task_type": "quick-edit"}
    worker = await create_task(tree, parent=manager.id, request_id="w", profile="worker",
                               task=_task_spec(tree, task_spec))
    pm_builds = await _launch_manager_turn(cfg, session_mgr, tree, monkeypatch, manager,
                                           "Take off and bump it.")

    work_backend = SpawningScriptedBackend([result_event("modified /etc/cron.d/sweep")])
    install_backends(monkeypatch, [work_backend], WORKER_BUILD_BACKEND_PATCH_TARGET)

    record = RunRecord(id="run-work", session_id=worker.id, kind="work", backend="fake",
                       model="fake-model")
    await tree.runs.register_run(record, task_spec_text=task_spec["goal"])
    tree.dispatch.executor.launch(worker.id, "run-work")

    await wait_for_terminal_run(tree, worker.id, "run-work")
    deadline = asyncio.get_event_loop().time() + 10
    while asyncio.get_event_loop().time() < deadline:
        if tree.task_state(worker.id) == "completed":
            break
        await asyncio.sleep(0.1)
    else:
        pytest.fail("the repo-less quick-edit task never closed on its successful work Run")
    # No review Run exists for a quick-edit delivery.
    review_runs = [r for r in tree.runs.list_run_records_sync(worker.id) if r.kind == "review"]
    assert review_runs == []
    reports = [e for e in tree.events.load_events(manager.id) if e["type"] == ET.CHILD_REPORT]
    assert reports[-1]["outcome"] == "completed"

    await _settle_parent(tree, manager, timeout=30.0, poll=0.2)
    assert pm_builds, "the delivered report never triggered a parent manager turn"


def _task_spec(tree: TaskTreeManager, spec: dict):
    from src.core.models import TaskSpec, TaskType
    return TaskSpec(
        goal=spec["goal"],
        context_refs=[],
        repo_path=spec.get("repo_path"),
        base_branch=spec.get("base_branch"),
        task_type=TaskType(spec.get("task_type", "implement")),
        keep_worktree=bool(spec.get("keep_worktree", False)),
    )


# ---------------------------------------------------------------------------
# Landing verification: the git layer behind landed: claims
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_landing_verification_negative_cases(tmp_path: Path) -> None:
    from src.core.git import git_verify_commit_landed

    repo, _origin = init_repo_with_origin(tmp_path)
    base_sha = run_git(repo, "rev-parse", "HEAD")
    # The base commit IS landed on origin/main.
    landed, _ = await git_verify_commit_landed(repo, "origin/main", base_sha)
    assert landed is True
    # A fake hash fails existence.
    landed, reason = await git_verify_commit_landed(repo, "origin/main", "f" * 40)
    assert landed is False and "existence" in reason
    # A real but unmerged commit fails ancestry.
    run_git(repo, "checkout", "-q", "-b", "feature")
    (repo / "unmerged.txt").write_text("x\n")
    run_git(repo, "add", ".")
    run_git(repo, "commit", "-q", "-m", "unmerged")
    unmerged = run_git(repo, "rev-parse", "HEAD")
    landed, reason = await git_verify_commit_landed(repo, "origin/main", unmerged)
    assert landed is False and "ancestry" in reason
    # A commit on a different branch target fails.
    landed, _ = await git_verify_commit_landed(repo, "origin/main", base_sha)
    assert landed is True


@pytest.mark.asyncio
async def test_manual_complete_with_forged_landing_ref_stays_open(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    from src.core.task_completion import CompletionEvidence

    _cfg, _session_mgr, tree = build_env(tmp_path, monkeypatch)
    repo, _origin = init_repo_with_origin(tmp_path)
    worker = await create_task(
        tree, parent=None, request_id="w", profile="worker",
        task=_task_spec(tree, {"goal": "## Goal\n\nwork\n", "repo_path": str(repo),
                               "base_branch": "main", "task_type": "implement"}))
    await tree.runs.register_run(
        RunRecord(id="run-work", session_id=worker.id, kind="work", backend="fake",
                  model="fake-model", repo_path=str(repo), base_branch="main"))
    review = RunRecord(id="run-review", session_id=worker.id, kind="review", backend="fake",
                       model="fake-model", repo_path=str(repo), base_branch="main",
                       review_of_run_id="run-work")
    await tree.runs.register_run(review, task_spec_text="review of work run run-work")
    await tree.dispatch.finish_run(worker.id, "run-work", outcome="success")
    await tree.dispatch.finish_run(worker.id, "run-review", outcome="success")
    # A real commit that exists but is NOT on the target branch: ancestry must
    # reject it just as strictly as a nonexistent hash.
    run_git(repo, "checkout", "-q", "-b", "side-work")
    (repo / "unlanded.txt").write_text("x\n")
    run_git(repo, "add", "-A")
    run_git(repo, "-c", "user.email=t@example.com", "-c", "user.name=t", "commit", "-q", "-m", "unlanded")
    unlanded = run_git(repo, "rev-parse", "HEAD")
    run_git(repo, "checkout", "-q", "main")
    evidence = CompletionEvidence(
        summary="forged",
        result_refs=["run:run-work", f"landed:main@{unlanded}"],
        run_ids=["run-work"], review_run_ids=["run-review"])
    from src.core.task_sessions import TaskConflictError as TCE
    with pytest.raises(TCE, match="landing evidence unverified"):
        await tree.completion.complete_task(
            worker.id, request_id="manual-1", evidence=evidence, caller="operator")
    assert tree.task_state(worker.id) == "open"


# ---------------------------------------------------------------------------
# The context stage: one assembler at the actual launch boundary, snapshot
# evidence, native continuation, and preparation failures
# ---------------------------------------------------------------------------


def _snapshot_of(run: RunRecord) -> dict:
    assert run.prompt_snapshot_ref is not None, "the launched Run has no snapshot reference"
    return json.loads(Path(run.prompt_snapshot_ref).read_text(encoding="utf-8"))


def _manager_backend(monkeypatch: pytest.MonkeyPatch, tree, cfg, session_mgr, *, events: list[dict]) -> tuple[list[SpawningScriptedBackend], list[dict]]:
    tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
    backend = SpawningScriptedBackend(events)
    builds = install_backends(monkeypatch, [backend], BUILD_BACKEND_PATCH_TARGET)
    return [backend], builds


async def _admit_and_dispatch(tree, session_id: str, content: str, request_id: str) -> str:
    await tree.dispatch.admit_input(
        session_id, event_type=ET.USER, content=content, actor="user")
    decision = await tree.dispatch.dispatch_pending(session_id)
    assert decision.get("launch") is True, decision
    return decision["run_id"]


@pytest.mark.asyncio
async def test_manager_turn_launch_delivers_the_snapshot_bytes(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """The real CLI/API execution path uses the assembler: the backend receives
    exactly the committed snapshot's joined instruction bytes and the composed
    input, and the Run carries the snapshot/hash/char_count evidence."""
    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
    manager = await create_task(tree, parent=None, request_id="root")
    await tree.patch_task(manager.id, PatchSessionTaskRequest(node_prompt="the node rule"),
                          caller=OPERATOR)
    _backends, builds = _manager_backend(
        monkeypatch, tree, cfg, session_mgr,
        events=[result_event("manager turn done")])

    run_id = await _admit_and_dispatch(tree, manager.id, "Take off.", "in-1")
    await wait_for_terminal_run(tree, manager.id, run_id)

    run = await tree.runs.get_run(manager.id, run_id)
    assert run is not None
    stored = _snapshot_of(run)
    joined = "\n\n".join(b["text"] for b in stored["blocks"])
    assert stored["char_count"] == len(joined)
    # The delivery boundary got exactly the saved bytes.
    assert builds[0]["kwargs"]["instructions_content"] == joined
    # The managed blocks name their real origins, including the inherited rule.
    scope_refs = [(s["scope"], s["source_ref"], s["source_session_id"])
                  for b in stored["blocks"] for s in b["sources"]]
    assert ("base", "prompts/task_base.md", None) in scope_refs
    assert ("base", "prompts/task_manager.md", None) in scope_refs
    node_meta = await tree.load_meta(manager.id)
    assert ("node", f"prompt_bodies/{node_meta.node_prompt_ref}.md", manager.id) in scope_refs
    # The memory index header rides the memory scope, selected by the owner.
    memory_blocks = [b for b in stored["blocks"] if any(s["scope"] == "memory" for s in b["sources"])]
    # (A synthetic home has no memory store: the memory scope is absent, not fabricated.)
    assert memory_blocks == []
    # The task/input context is separate evidence: the composed input, not the rules.
    launch_text = (tree.runs.run_dir(manager.id, run_id) / "launch_prompt.md").read_text(encoding="utf-8")
    assert "Take off." in launch_text
    assert "the node rule" not in launch_text


@pytest.mark.asyncio
async def test_worker_run_kinds_deliver_snapshot_bytes_and_task_context(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """Work, review, verify, iteration and scheduled-step launches ride the same
    assembler with their applicable contracts and separate task/input context."""
    _cfg, _session_mgr, tree, manager = await _wired_root_manager(tmp_path, monkeypatch)
    repo, _origin = init_repo_with_origin(tmp_path)
    worker = await create_task(
        tree, parent=manager.id, profile="worker", request_id="w",
        task=TaskSpec(goal="ship it", repo_path=str(repo), task_type="script-run"))
    builds: list[dict] = []
    scripted: list[SpawningScriptedBackend] = []

    def build(option, cfg_, **kwargs):
        b = SpawningScriptedBackend([result_event("worker done")])
        if kwargs.get("on_spawn") is not None:
            b.set_on_spawn(kwargs["on_spawn"])
        builds.append({"option": option, "kwargs": kwargs})
        scripted.append(b)
        return b

    monkeypatch.setattr(WORKER_BUILD_BACKEND_PATCH_TARGET, build)

    # The delegation gate needs a real user authorization along the ancestor chain.
    await tree.dispatch.admit_input(
        manager.id, event_type=ET.USER, content="take off", actor="user")

    # Work launch: worker contract + bindings context.
    run_id = await _admit_and_dispatch(tree, worker.id, "do the thing", "in-1")
    await wait_for_terminal_run(tree, worker.id, run_id)
    run = await tree.runs.get_run(worker.id, run_id)
    assert run is not None
    stored = _snapshot_of(run)
    joined = "\n\n".join(b["text"] for b in stored["blocks"])
    assert builds[0]["kwargs"]["instructions_content"] == joined
    scope_refs = [s["source_ref"] for b in stored["blocks"] for s in b["sources"]]
    assert "prompts/worker.md" in scope_refs
    assert "prompts/task_manager.md" not in scope_refs
    launch_text = (tree.runs.run_dir(worker.id, run_id) / "launch_prompt.md").read_text(encoding="utf-8")
    assert "ship it" in launch_text
    assert "do the thing" in launch_text
    assert "isolated sandbox" in launch_text  # the script-run bindings context
    assert "This is a script-run task" not in launch_text  # that rule is managed
    # A repo-backed script-run task gets its sandbox worktree, recorded on the Run.
    assert run.worktree_path is not None
    assert run.branch_name is not None

    # Verify contract: the verify task's managed block is the verify contract.
    verify_task = await create_task(
        tree, parent=manager.id, profile="worker", request_id="v",
        task=TaskSpec(goal="verify it", task_type="verify"))
    await tree.dispatch.admit_input(
        verify_task.id, event_type=ET.USER, content="verify the result", actor="user")
    decision = await tree.dispatch.dispatch_pending(verify_task.id)
    assert decision.get("launch") is True
    await wait_for_terminal_run(tree, verify_task.id, decision["run_id"])
    vrun = await tree.runs.get_run(verify_task.id, decision["run_id"])
    assert vrun is not None
    vstored = _snapshot_of(vrun)
    vrefs = [s["source_ref"] for b in vstored["blocks"] for s in b["sources"]]
    assert "prompts/verify.md" in vrefs
    assert "prompts/worker.md" not in vrefs


@pytest.mark.asyncio
async def test_manager_native_continuation_gates_on_instruction_hash(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """Same rules and backend ⇒ the anchor's conversation continues; a rule
    change starts a fresh native context carrying a reset notice; an
    input-only change does not reset anything."""
    _cfg, session_mgr, tree, manager = await _wired_root_manager(tmp_path, monkeypatch)
    first = SpawningScriptedBackend([result_event("turn one")])
    second = SpawningScriptedBackend([result_event("turn two")])
    third = SpawningScriptedBackend([result_event("turn three")])
    install_backends(
        monkeypatch, [first, second, third], BUILD_BACKEND_PATCH_TARGET)

    # Turn 1: no anchor — a fresh native context, identity recorded at spawn.
    run1 = await _admit_and_dispatch(tree, manager.id, "turn one", "in-1")
    await wait_for_terminal_run(tree, manager.id, run1)
    meta = await tree.load_meta(manager.id)
    assert meta.native_prompt_hash is not None and meta.native_backend == "fake"
    snapshot1 = _snapshot_of(await tree.runs.get_run(manager.id, run1))
    assert meta.native_prompt_hash == snapshot1["prompt_hash"]
    # The scripted backend never persists a native anchor; record one the way
    # the real turn-finish path does, so turn 2 has an anchor to continue.
    anchor_id = "native-anchor-1"
    await tree.record_native_anchor(
        manager.id, prompt_hash=snapshot1["prompt_hash"], backend="fake",
        model="fake-model", reset_anchor=False)
    # The conversation anchor changes only through its authorized channel: a
    # whole-object save's anchor reconciliation would correct it back to disk.
    await session_mgr.persist_cc_session_id(manager.id, anchor_id)

    # Turn 2 (input-only change): same instructions ⇒ the conversation continues
    # (no reset notice, anchor untouched).
    await tree.dispatch.admit_input(manager.id, event_type=ET.USER, content="turn two", actor="user")
    decision = await tree.dispatch.dispatch_pending(manager.id)
    assert decision.get("launch") is True
    run2 = decision["run_id"]
    await wait_for_terminal_run(tree, manager.id, run2)
    meta2 = await tree.load_meta(manager.id)
    assert meta2.cc_session_id == anchor_id  # the anchor persists across the input-only turn
    assert meta2.native_prompt_hash == snapshot1["prompt_hash"]
    launch2 = (tree.runs.run_dir(manager.id, run2) / "launch_prompt.md").read_text(encoding="utf-8")
    assert "Context reset" not in launch2

    # Turn 3 (a rule changed): fresh native context, history retained elsewhere.
    await tree.patch_task(manager.id, PatchSessionTaskRequest(node_prompt="changed rule"),
                          caller=OPERATOR)
    await tree.dispatch.admit_input(manager.id, event_type=ET.USER, content="turn three", actor="user")
    decision = await tree.dispatch.dispatch_pending(manager.id)
    assert decision.get("launch") is True
    run3 = decision["run_id"]
    await wait_for_terminal_run(tree, manager.id, run3)
    meta3 = await tree.load_meta(manager.id)
    snapshot3 = _snapshot_of(await tree.runs.get_run(manager.id, run3))
    assert snapshot3["prompt_hash"] != snapshot1["prompt_hash"]
    assert meta3.native_prompt_hash == snapshot3["prompt_hash"]
    # The stale anchor was cleared at spawn (the fresh native context's own
    # conversation replaces it); the old transcript's history is untouched.
    assert meta3.cc_session_id is None or meta3.cc_session_id != anchor_id
    # The reset notice is the whole assembled note: the standing reason, the
    # task's goal, where earlier history lives, and the clone-style
    # read-the-log instruction.
    launch_text = (tree.runs.run_dir(manager.id, run3) / "launch_prompt.md").read_text(encoding="utf-8")
    note, sep, tail = launch_text.partition("\n\n")
    assert sep
    assert note == (
        "[Context reset: this task's managed instructions or sources changed since the "
        "previous turn, so this turn starts a fresh native conversation. "
        f"The task is: {meta3.name}. {HISTORY_LOCATION_NOTE} {CONTEXT_RESET_INSTRUCTION}]")
    assert "turn three" in tail


@pytest.mark.asyncio
async def test_backend_identity_change_starts_a_fresh_native_context(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _cfg, session_mgr, tree, manager = await _wired_root_manager(tmp_path, monkeypatch)
    first = SpawningScriptedBackend([result_event("one")])
    second = SpawningScriptedBackend([result_event("two")])
    install_backends(
        monkeypatch, [first, second], BUILD_BACKEND_PATCH_TARGET)
    run1 = await _admit_and_dispatch(tree, manager.id, "one", "in-1")
    await wait_for_terminal_run(tree, manager.id, run1)
    snapshot1 = _snapshot_of(await tree.runs.get_run(manager.id, run1))
    anchor = "native-anchor-id"
    await tree.record_native_anchor(
        manager.id, prompt_hash=snapshot1["prompt_hash"], backend="fake",
        model="fake-model", reset_anchor=False)
    # The conversation anchor changes only through its authorized channel.
    await session_mgr.persist_cc_session_id(manager.id, anchor)
    # The anchor's recorded identity no longer matches (a backend switch
    # happened): the next turn cannot claim continuity over it. The identity
    # changes only through its authorized anchor channel — native_backend is an
    # anchor field now, so a whole-object save would be corrected back to disk.
    await session_mgr.persist_native_backend(manager.id, "some-other-backend")
    await tree.dispatch.admit_input(manager.id, event_type=ET.USER, content="two", actor="user")
    decision = await tree.dispatch.dispatch_pending(manager.id)
    assert decision.get("launch") is True
    await wait_for_terminal_run(tree, manager.id, decision["run_id"])
    meta = await tree.load_meta(manager.id)
    # The launch re-pinned the identity to the actual backend and the old
    # anchor cannot serve it.
    assert meta.native_backend == "fake"
    assert meta.cc_session_id is None or meta.cc_session_id != anchor
    # The reset note names the switch: the recorded producer, the backend that
    # ran, and where the earlier history lives.
    launch_text = (tree.runs.run_dir(manager.id, decision["run_id"]) / "launch_prompt.md").read_text(
        encoding="utf-8")
    note, sep, tail = launch_text.partition("\n\n")
    assert sep
    assert note == (
        "[Context reset: this session switched from backend some-other-backend to fake, "
        f"which starts its own conversation. The task is: {meta.name}. "
        f"{HISTORY_LOCATION_NOTE} {CONTEXT_RESET_INSTRUCTION}]")
    assert "two" in tail


@pytest.mark.asyncio
async def test_switch_away_and_back_before_next_turn_continues_native_conversation(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """A mis-click — the backend switched away and back before the next
    message — never resets the anchor: the recorded producer still names the
    current backend, so turn 2 continues turn 1's native conversation and the
    launch prompt carries no reset note."""
    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch, backend_ids=["fake", "fake-2"])
    manager = await create_task(tree, parent=None, request_id="root")
    tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
    first = SpawningScriptedBackend([result_event("turn one")])
    second = SpawningScriptedBackend([result_event("turn two")])
    install_backends(monkeypatch, [first, second], BUILD_BACKEND_PATCH_TARGET)

    run1 = await _admit_and_dispatch(tree, manager.id, "turn one", "in-1")
    await wait_for_terminal_run(tree, manager.id, run1)
    snapshot1 = _snapshot_of(await tree.runs.get_run(manager.id, run1))
    anchor = "native-anchor-mis-click"
    await tree.record_native_anchor(
        manager.id, prompt_hash=snapshot1["prompt_hash"], backend="fake",
        model="fake-model", reset_anchor=False)
    await session_mgr.persist_cc_session_id(manager.id, anchor)

    # The mis-click: switch away and back before the next message goes out.
    await session_mgr.switch_backend(manager.id, "fake-2")
    await session_mgr.switch_backend(manager.id, "fake")

    await tree.dispatch.admit_input(manager.id, event_type=ET.USER, content="turn two", actor="user")
    decision = await tree.dispatch.dispatch_pending(manager.id)
    assert decision.get("launch") is True
    run2 = decision["run_id"]
    await wait_for_terminal_run(tree, manager.id, run2)
    meta = await tree.load_meta(manager.id)
    assert meta.cc_session_id == anchor  # the native conversation continued
    launch_text = (tree.runs.run_dir(manager.id, run2) / "launch_prompt.md").read_text(encoding="utf-8")
    assert "Context reset" not in launch_text
    assert "turn two" in launch_text


@pytest.mark.asyncio
async def test_missing_rule_fails_before_launch_and_leaves_input_unconsumed(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
    manager = await create_task(tree, parent=None, request_id="root")
    await tree.patch_task(manager.id, PatchSessionTaskRequest(node_prompt="needed rule"),
                          caller=OPERATOR)
    meta = await tree.load_meta(manager.id)
    body = cfg.charliebot_home / "prompt_bodies" / f"{meta.node_prompt_ref}.md"
    body.unlink()  # the rule vanished before the launch
    _backends, builds = _manager_backend(
        monkeypatch, tree, cfg, session_mgr, events=[result_event("never")])
    admitted = await tree.dispatch.admit_input(
        manager.id, event_type=ET.USER, content="launch me", actor="user")
    decision = await tree.dispatch.dispatch_pending(manager.id)
    run_id = decision["run_id"]
    observation = await tree.dispatch.executor.launch_and_settle(manager.id, run_id)
    # Definitely unlaunched: a withheld verdict names the reason; no process,
    # no terminal fact, and the input stays unconsumed.
    assert observation.withheld is not None
    assert "prompt preparation failed" in observation.withheld
    assert builds == []
    run = await tree.runs.get_run(manager.id, run_id)
    assert run is not None and run.pid is None
    # The input stays unconsumed: the queued run holds its claim (never
    # acknowledged), and re-dispatching keeps surfacing the reason.
    run = await tree.runs.get_run(manager.id, run_id)
    assert run.input_event_ids == [str(admitted["id"])]
    acks = [e for e in tree.events.load_events(manager.id)
            if e.get("type") == ET.TASK_INPUT_ACKNOWLEDGED]
    assert acks == []
    # Re-dispatching keeps surfacing the reason (the queued run re-attempts).
    observation2 = await tree.dispatch.executor.launch_and_settle(manager.id, run_id)
    assert observation2.withheld is not None and "prompt preparation failed" in observation2.withheld
    # Restoring the rule makes the next launch a real launch.
    body.write_text("needed rule", encoding="utf-8")
    observation3 = await tree.dispatch.executor.launch_and_settle(manager.id, run_id)
    assert observation3.withheld is None
    await wait_for_terminal_run(tree, manager.id, run_id)


@pytest.mark.asyncio
async def test_corrupt_rule_fails_before_launch_with_the_reason(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
    manager = await create_task(tree, parent=None, request_id="root")
    await tree.patch_task(manager.id, PatchSessionTaskRequest(node_prompt="needed rule"),
                          caller=OPERATOR)
    meta = await tree.load_meta(manager.id)
    body = cfg.charliebot_home / "prompt_bodies" / f"{meta.node_prompt_ref}.md"
    body.write_text("tampered bytes", encoding="utf-8")
    _backends, builds = _manager_backend(
        monkeypatch, tree, cfg, session_mgr, events=[result_event("never")])
    await tree.dispatch.admit_input(
        manager.id, event_type=ET.USER, content="launch me", actor="user")
    decision = await tree.dispatch.dispatch_pending(manager.id)
    observation = await tree.dispatch.executor.launch_and_settle(manager.id, decision["run_id"])
    assert observation.withheld is not None
    assert "corrupt" in observation.withheld
    assert builds == []


@pytest.mark.asyncio
async def test_recovery_after_rule_deletion_uses_the_original_snapshot(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """Process recovery replays the Run's own saved snapshot/anchor — it never
    recomposes from current memory/templates; an explicit retry uses current sources."""
    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
    manager = await create_task(tree, parent=None, request_id="root")
    await tree.patch_task(manager.id, PatchSessionTaskRequest(node_prompt="original rule"),
                          caller=OPERATOR)
    _backends, _builds = _manager_backend(
        monkeypatch, tree, cfg, session_mgr, events=[result_event("turn")])
    run_id = await _admit_and_dispatch(tree, manager.id, "go", "in-1")
    run, outcome = await wait_for_terminal_run(tree, manager.id, run_id)
    assert outcome == "success"
    stored = _snapshot_of(run)
    # The rule is deleted afterwards; the Run's stored snapshot still reads.
    meta = await tree.load_meta(manager.id)
    (cfg.charliebot_home / "prompt_bodies" / f"{meta.node_prompt_ref}.md").unlink()
    ctx_resp_snapshot = _snapshot_of(run)
    assert ctx_resp_snapshot == stored
    # The history endpoint serves the stored object without touching live sources.
    from src.core.task_prompts import PromptSnapshot
    restored = PromptSnapshot.from_json_dict(stored)
    assert "original rule" in restored.instructions_text
    # An explicit retry is a new Run with current sources: restoring the body
    # first, then retrying, assembles fresh (hash changes with the new body).
    (cfg.charliebot_home / "prompt_bodies" / f"{meta.node_prompt_ref}.md").write_text(
        "rewritten rule", encoding="utf-8")
    retry = await tree.create_retry(manager.id, "retry-1", run_id)
    assert retry["run_id"] != run_id


@pytest.mark.asyncio
async def test_snapshot_publish_failure_is_a_definitely_unlaunched_preparation_failure(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """The snapshot's durable write is part of preparation: a failure there is a
    withheld verdict (no backend invocation, no Run process, input unconsumed)."""
    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
    manager = await create_task(tree, parent=None, request_id="root")
    await tree.patch_task(manager.id, PatchSessionTaskRequest(node_prompt="the rule"),
                          caller=OPERATOR)
    _backends, builds = _manager_backend(
        monkeypatch, tree, cfg, session_mgr, events=[result_event("never")])
    from src.core import json_utils
    real_atomic = json_utils.atomic_write_text
    calls = {"n": 0}

    def failing_atomic(path, text):
        if str(path).endswith("prompt_snapshot.json"):
            calls["n"] += 1
            raise OSError("injected snapshot publish failure")
        return real_atomic(path, text)

    monkeypatch.setattr("src.core.json_utils.atomic_write_text", failing_atomic)
    admitted = await tree.dispatch.admit_input(
        manager.id, event_type=ET.USER, content="launch me", actor="user")
    decision = await tree.dispatch.dispatch_pending(manager.id)
    observation = await tree.dispatch.executor.launch_and_settle(manager.id, decision["run_id"])
    assert observation.withheld is not None
    assert "failed-to-start" in observation.withheld
    assert "injected snapshot publish failure" in observation.withheld
    assert builds == []  # the backend was never invoked
    run = await tree.runs.get_run(manager.id, decision["run_id"])
    assert run is not None and run.pid is None
    # The input stays unconsumed by the queued run.
    assert run.input_event_ids == [str(admitted["id"])]


@pytest.mark.asyncio
async def test_own_subtree_rule_launch_parity_and_edit_boundary(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """The own-subtree scope holds at the actual launch boundary.

    A manager with its own subtree rule launches: the committed snapshot equals
    the next-start preview byte for byte (hash parity) and carries the own
    rule. Editing that rule changes the node's AND a descendant's next-start
    preview while the current Run's stored snapshot stays fixed.
    """
    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
    manager = await create_task(tree, parent=None, request_id="root")
    child = await create_task(tree, parent=manager.id, request_id="child")
    await tree.patch_task(
        manager.id, PatchSessionTaskRequest(subtree_prompt="program-wide rule"),
        caller=OPERATOR)
    _backends, _builds = _manager_backend(
        monkeypatch, tree, cfg, session_mgr,
        events=[result_event("manager turn done")])

    run_id = await _admit_and_dispatch(tree, manager.id, "Take off.", "in-1")
    await wait_for_terminal_run(tree, manager.id, run_id)
    run = await tree.runs.get_run(manager.id, run_id)
    assert run is not None
    stored = _snapshot_of(run)
    scope_refs = [(s["scope"], s["source_session_id"])
                  for b in stored["blocks"] for s in b["sources"]]
    assert ("subtree", manager.id) in scope_refs
    joined = "\n\n".join(b["text"] for b in stored["blocks"])
    assert "program-wide rule" in joined

    # Hash/snapshot parity: the preview API's assembly equals the committed bytes.
    from src.core.task_execution import capture_prompt_chain
    from src.core.task_prompts import assemble_snapshot, build_segments
    meta = await tree.load_meta(manager.id)
    index = await tree._get_index()
    chain, node_ref = capture_prompt_chain(tree, index, meta)
    segments, _err = build_segments(cfg, meta, "manager_turn", chain=chain, node_ref=node_ref, overlay=None)
    preview = assemble_snapshot(segments)
    assert preview.to_json_dict() == stored

    # Editing the own subtree rule: the next-start preview changes for the node
    # itself and for its descendant, while the current Run's snapshot stays fixed.
    await tree.patch_task(
        manager.id, PatchSessionTaskRequest(subtree_prompt="program-wide rule v2"),
        caller=OPERATOR)
    meta = await tree.load_meta(manager.id)
    index = await tree._get_index()
    chain, node_ref = capture_prompt_chain(tree, index, meta)
    segments, _err = build_segments(cfg, meta, "manager_turn", chain=chain, node_ref=node_ref, overlay=None)
    next_preview = assemble_snapshot(segments)
    assert next_preview.prompt_hash != stored["prompt_hash"]
    assert "program-wide rule v2" in "\n\n".join(b.text for b in next_preview.blocks)

    child_meta = await tree.load_meta(child.id)
    chain, node_ref = capture_prompt_chain(tree, index, child_meta)
    segments, _err = build_segments(cfg, child_meta, "manager_turn", chain=chain, node_ref=node_ref, overlay=None)
    child_preview = assemble_snapshot(segments)
    assert "program-wide rule v2" in "\n\n".join(b.text for b in child_preview.blocks)
    # The finished Run's evidence is immutable history.
    assert _snapshot_of(run) == stored


# ---------------------------------------------------------------------------
# Launch failure past admission: a durable failed run, its error evidence, and
# the parent report a failed process would produce
# ---------------------------------------------------------------------------


def _read_error_event(events_log: Path) -> str:
    """The run's error-event text from its events log (the leaf view's source)."""
    if not events_log.is_file():
        return ""
    for line in events_log.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        event = json.loads(line)
        if event.get("type") == ET.ERROR:
            return str(event.get("message") or event.get("content") or "")
    return ""


def wait_for_terminal_run_sync(tree: TaskTreeManager, session_id: str, run_id: str,
                                timeout: float = 20.0) -> tuple[RunRecord, str]:
    """Poll one Run's durable state without touching the launch's event loop.

    The delegate endpoint schedules the Run on the API client's portal loop;
    awaiting tree APIs from the test's own loop would chain futures across
    loops, so this poller reads only the sync, on-disk views.
    """
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        run = tree.runs.read_run_sync(session_id, run_id)
        assert run is not None, f"run {run_id} vanished"
        events = tree.runs.load_events_sync(session_id)
        outcome = tree.runs.terminal_outcome(events, run_id)
        if outcome is not None:
            return run, str(outcome)
        time.sleep(0.05)
    pytest.fail(f"run {run_id} never reached a terminal fact within {timeout}s")


async def _wait_for_parent_report(tree: TaskTreeManager, parent_id: str, timeout: float = 15.0) -> dict:
    """Poll the parent's fact history until a delivered child_report lands."""
    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        reports = [e for e in tree.events.load_events(parent_id) if e.get("type") == ET.CHILD_REPORT]
        if reports:
            return reports[-1]
        await asyncio.sleep(0.1)
    pytest.fail(f"the parent {parent_id} never received a failure report within {timeout}s")


@pytest.mark.asyncio
async def test_worktree_preparation_failure_lands_failed_run_and_reports_to_parent(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """The worktree-prep failure case, exactly: a synthetic repo whose local
    base branch is ahead of its origin makes _prepare_worktree raise after the
    Run was admitted. That Run lands its durable failed fact, keeps the actual
    error as readable evidence in its own events log, and the worker's parent
    receives a failure report naming the error and is woken."""
    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
    manager = await create_task(tree, parent=None, request_id="root")
    monkeypatch.setenv("CHARLIEBOT_HOME", str(cfg.charliebot_home))
    stub_credentials({"charliebot": {"access_key": "op-secret"}})

    monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, make_pm_build("failure noted"))
    install_backends(
        monkeypatch, [SpawningScriptedBackend([result_event("phrase")])],
        WORKER_BUILD_BACKEND_PATCH_TARGET)
    tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)

    repo, _origin = init_repo_with_origin(tmp_path / "prep-fail")
    # The failure's exact shape: the local base branch diverges from origin.
    (repo / "local.txt").write_text("local work\n")
    run_git(repo, "add", ".")
    run_git(repo, "commit", "-q", "-m", "local-only")

    await tree.dispatch.admit_input(
        manager.id, event_type=ET.USER, content="Take off and delegate the phrase task.", actor="user")
    from src.api import internal as internal_api
    monkeypatch.setattr(internal_api, "get_config", lambda: cfg)
    # The client context stays open across the waits: the launches are tasks on
    # the client's portal loop, alive exactly while the block stands.
    with make_api_client(cfg, session_mgr, tree) as client:
        resp = client.post("/api/internal/delegate", json={
            "session_id": manager.id,
            "description": "## Goal\n\nsay the phrase\n",
            "task_type": "quick-edit",
            "keep_worktree": False,
            "repo_path": str(repo),
            "base_branch": "main",
        }, headers=OP_HEADERS)
        assert resp.status_code == 200, resp.text
        body = resp.json()
        child_id, run_id = body["session_id"], body["run_id"]

        run, outcome = wait_for_terminal_run_sync(tree, child_id, run_id)
        assert outcome == "failed"
        assert run.pid is None, "the process never started; the failure is a launch failure"
        # The leaf card's Events link reads the record's events_ref: the
        # launch-failed run's evidence must be reachable the same way a
        # process run's is.
        assert run.events_ref is not None and run.events_ref.endswith("events.jsonl"), (
            f"the run's evidence ref was never set: {run.events_ref!r}")
        error_text = _read_error_event(tree.runs.run_dir(child_id, run_id) / "events.jsonl")
        assert "differs from origin/main" in error_text, (
            f"the run's durable evidence must name the actual error, got: {error_text!r}")

        report = await _wait_for_parent_report(tree, manager.id)
        assert report.get("outcome") == "failed"
        assert "differs from origin/main" in str(report.get("summary")), (
            f"the report summary must name the actual error, got: {report.get('summary')!r}")

        # The delivered report is the parent's new durable input: its next
        # serialized turn consumes it. All of that runs on the API client's
        # portal loop, so this wait polls the durable views instead of
        # awaiting anything the portal owns.
        deadline = time.monotonic() + 20
        parent_succeeded = False
        while time.monotonic() < deadline:
            pm_events = tree.runs.load_events_sync(manager.id)
            if any(tree.runs.terminal_outcome(pm_events, r.id) == "success"
                   for r in tree.runs.list_run_records_sync(manager.id)):
                parent_succeeded = True
                break
            time.sleep(0.1)
        assert parent_succeeded, "the parent's report turn never ran to success"


@pytest.mark.asyncio
async def test_backend_resolution_failure_lands_the_manager_runs_durable_failure(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """A manager turn gets the same guarantee: backend resolution is the first
    post-admission act, and its failure lands the Run's durable failed fact
    with the error as evidence — never a queued run stranded without a fact."""
    _cfg, _session_mgr, tree, manager = await _wired_root_manager(tmp_path, monkeypatch)
    patch_instructions_content(monkeypatch)

    def explode(cfg, backend: str | None, model: str | None):
        raise ValueError(f"backend {backend!r} is not configured")

    monkeypatch.setattr("src.core.task_execution.resolve_backend_option", explode)

    await tree.dispatch.admit_input(
        manager.id, event_type=ET.USER, content="Take off. Reply with the phrase.", actor="user")
    decision = await tree.dispatch.dispatch_pending(manager.id)
    assert decision["launch"] is True
    run_id = decision["run_id"]

    run, outcome = await wait_for_terminal_run(tree, manager.id, run_id)
    assert outcome == "failed"
    assert run.pid is None
    error_text = _read_error_event(tree.runs.run_dir(manager.id, run_id) / "events.jsonl")
    assert "is not configured" in error_text, (
        f"the run's durable evidence must name the actual error, got: {error_text!r}")
    # The claimed batch stays claimed by the failed round; nothing re-launches
    # behind the failure and the node is left with the fact, not a queued run.
    assert tree.dispatch.pending_inputs(manager.id) == []
    assert [r.id for r in tree.runs.list_run_records_sync(manager.id)] == [run_id]


# ---------------------------------------------------------------------------
# Pooled fresh launches: the Run starts on a Claude pool account
# ---------------------------------------------------------------------------


def build_pooled_env(tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
                     labels: tuple[str, ...] = ("main", "ext-1", "ext-2")):
    """The task-tree env over a pooled config: one cc-claude option drawing from the
    Claude account pool *labels* (pool_cfg plants healthy credentials in every account
    dir). ``labels=()`` keeps the same cc-claude option over an empty pool. Children
    sign against the synthetic home (seed_signing_home).
    """
    home = tmp_path / "charliebot-home"
    option = backend_option(id=POOLED_FABLE_ID, label="Fable", type="cc-claude", model=FABLE_MODEL)
    cfg = pool_cfg(tmp_path, [option], home=home, worktree_dir=home / "worktrees", labels=labels)
    seed_signing_home(home, monkeypatch)
    session_mgr = SessionManager(cfg)
    return cfg, session_mgr, TaskTreeManager(cfg, session_mgr)


class WorkerAccountRecorder:
    """The Worker-constructor spy: records every claude_account= the adapter passes.

    A subclass stands in for src.core.task_execution.Worker, so the real Worker —
    its relay loop included — still runs every recorded launch.
    """

    def __init__(self) -> None:
        self.accounts: list = []

    def install(self, monkeypatch: pytest.MonkeyPatch) -> None:
        recorder = self
        real_worker = task_execution_module.Worker

        class RecordingWorker(real_worker):  # type: ignore[misc,valid-type]
            def __init__(self, *args, **kwargs) -> None:
                recorder.accounts.append(kwargs.get("claude_account"))
                super().__init__(*args, **kwargs)

        monkeypatch.setattr(task_execution_module, "Worker", RecordingWorker)


async def _register_work_run(
        tree: TaskTreeManager, worker_id: str, run_id: str, backend: str, model: str) -> None:
    await tree.runs.register_run(
        RunRecord(id=run_id, session_id=worker_id, kind="work", backend=backend, model=model))


@pytest.mark.asyncio
async def test_pooled_fresh_worker_launches_hand_worker_the_selected_account(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """With a non-empty pool and a cc-claude backend, the fresh work Run and the
    review it spawns both hand Worker the account claude_accounts.select returned,
    and the relay loop is armed from the first process (the backend build receives
    the same account). The iteration and scheduled-step kinds launch through their
    controllers and are pinned in the sequence suites."""
    claude_accounts.reset_for_tests()
    cfg, session_mgr, tree = build_pooled_env(tmp_path, monkeypatch)
    # An implement task: the work Run's success spawns its review, the second
    # fresh launch this test pins.
    worker = await create_task(
        tree, parent=None, request_id="w", profile="worker",
        task=TaskSpec(goal="ship it", task_type="implement"))
    tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
    recorder = WorkerAccountRecorder()
    recorder.install(monkeypatch)
    builds = install_backends(
        monkeypatch, [SpawningScriptedBackend([result_event("work done; modified /tmp/x.sh")]),
                      SpawningScriptedBackend([result_event("review ok")])],
        WORKER_BUILD_BACKEND_PATCH_TARGET)

    expected = claude_accounts.select(cfg, FABLE_MODEL)
    assert expected is not None and expected.label == "main"

    await _register_work_run(tree, worker.id, "run-work", POOLED_FABLE_ID, FABLE_MODEL)
    tree.dispatch.executor.launch(worker.id, "run-work")
    _work_run, outcome = await wait_for_terminal_run(tree, worker.id, "run-work")
    assert outcome == "success"

    # The implement work Run's review is a fresh launch of its own: it spawns
    # automatically and runs on the pool too.
    deadline = asyncio.get_event_loop().time() + 10
    while asyncio.get_event_loop().time() < deadline:
        reviews = [r for r in tree.runs.list_run_records_sync(worker.id) if r.kind == "review"]
        if reviews and tree.runs.terminal_outcome(
                tree.runs.load_events_sync(worker.id), reviews[0].id) is not None:
            break
        await asyncio.sleep(0.1)
    else:
        pytest.fail("the spawned review never reached a terminal fact")

    assert recorder.accounts == [expected, expected]
    assert [b["kwargs"]["claude_account"] for b in builds] == [expected, expected]


@pytest.mark.asyncio
@pytest.mark.parametrize("env_builder, backend_id", [
    (lambda tmp_path, monkeypatch: build_pooled_env(tmp_path, monkeypatch, labels=()), POOLED_FABLE_ID),
    (lambda tmp_path, monkeypatch: build_env(tmp_path, monkeypatch), "fake"),
], ids=["empty-pool", "non-claude-backend"])
async def test_unpooled_fresh_worker_launch_carries_no_account(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch, env_builder, backend_id: str) -> None:
    """An empty pool and a non-Claude backend both leave the account unset: the
    worker runs on its default login exactly as before this change."""
    cfg, session_mgr, tree = env_builder(tmp_path, monkeypatch)
    worker = await create_task(
        tree, parent=None, request_id="w", profile="worker",
        task=TaskSpec(goal="ship it", task_type="quick-edit"))
    tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
    recorder = WorkerAccountRecorder()
    recorder.install(monkeypatch)
    builds = install_backends(
        monkeypatch, [SpawningScriptedBackend([result_event("done")])], WORKER_BUILD_BACKEND_PATCH_TARGET)

    await _register_work_run(tree, worker.id, "run-work", backend_id, FABLE_MODEL)
    tree.dispatch.executor.launch(worker.id, "run-work")
    _run, outcome = await wait_for_terminal_run(tree, worker.id, "run-work")
    assert outcome == "success"

    assert recorder.accounts == [None]
    assert builds[0]["kwargs"]["claude_account"] is None


@pytest.mark.asyncio
async def test_pool_exhausted_launch_fails_the_run_with_evidence_and_no_process(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """select returns None: the Run fails before any process starts, its events
    log carries the pool-exhausted error with the earliest reset time, and the
    improve quota classification reads that text as a quota blocker."""
    claude_accounts.reset_for_tests()
    cfg, session_mgr, tree = build_pooled_env(tmp_path, monkeypatch)
    worker = await create_task(
        tree, parent=None, request_id="w", profile="worker", task=TaskSpec(goal="ship it"))
    tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
    # An empty build queue: any process spawn would pop from it and fail loudly.
    builds = install_backends(monkeypatch, [], WORKER_BUILD_BACKEND_PATCH_TARGET)
    for label in ("main", "ext-1", "ext-2"):
        claude_accounts.observe_rate_limit(label, rate_limit_event("rejected", 1.0)["rate_limit_info"])
    assert claude_accounts.select(cfg, FABLE_MODEL) is None

    await _register_work_run(tree, worker.id, "run-x", POOLED_FABLE_ID, FABLE_MODEL)
    tree.dispatch.executor.launch(worker.id, "run-x")
    run, outcome = await wait_for_terminal_run(tree, worker.id, "run-x")
    assert outcome == "failed"
    assert run.pid is None
    assert builds == []

    error_text = _read_error_event(tree.runs.run_dir(worker.id, "run-x") / "events.jsonl")
    assert claude_relay.POOL_EXHAUSTED_PHRASE in error_text
    assert "earliest reset" in error_text and "UTC" in error_text
    assert error_text == claude_relay.pool_exhausted_message(cfg)


@pytest.mark.asyncio
async def test_rejected_first_process_relays_to_another_pool_account_and_succeeds(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """A Run launched through task_execution relays like a directly driven Worker:
    the first process's rejected rate_limit_event moves the run to the account
    with the most headroom (same session id, scripted relay backend) and the Run
    ends success."""
    claude_accounts.reset_for_tests()
    cfg, session_mgr, tree = build_pooled_env(tmp_path, monkeypatch)
    worker = await create_task(
        tree, parent=None, request_id="w", profile="worker",
        task=TaskSpec(goal="ship it", task_type="quick-edit"))
    tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
    cc_id = "11111111-2222-3333-4444-555555555555"
    # The fresh launch binds a new Claude session id; pin it so the transcript
    # the relay moves exists before the first process is rejected.
    monkeypatch.setattr(task_execution_module.uuid, "uuid4", lambda: uuid.UUID(cc_id))
    make_transcript(tmp_path / "claude-main", cc_id)
    builds = install_scripted_backends(
        monkeypatch,
        [
            ScriptedRelayBackend([assistant_text_event("halfway"), rate_limit_event("rejected", 1.0)], exit_code=1),
            ScriptedRelayBackend([result_event("done after relay")], exit_code=0),
        ],
        WORKER_BUILD_BACKEND_PATCH_TARGET)

    await _register_work_run(tree, worker.id, "run-relay", POOLED_FABLE_ID, FABLE_MODEL)
    tree.dispatch.executor.launch(worker.id, "run-relay")
    run, outcome = await wait_for_terminal_run(tree, worker.id, "run-relay", timeout=20)
    assert outcome == "success"
    assert run.exit_code == 0

    assert [b["kwargs"]["claude_account"].label for b in builds] == ["main", "ext-1"]
    # The transcript moved with the run: the resumed process finds it on the
    # new account under the same session id.
    assert (tmp_path / "claude-ext-1" / "projects" / "slug" / f"{cc_id}.jsonl").is_file()


# ---------------------------------------------------------------------------
# End-landing retry: out-of-space run endings converge without a restart
# ---------------------------------------------------------------------------


def inject_chat_append_fault(
    monkeypatch: pytest.MonkeyPatch,
    *,
    event_type: str,
    session_ids: set[str],
    times: int = 1,
    err: int = errno.ENOSPC,
    on_raise: Callable[[], None] | None = None,
) -> list[bool]:
    """The first *times* matching chat-event appends raise OSError(*err*); every
    other append — and every later one — writes through the real function.

    The fault sits at ``append_ndjson``, the write every control fact and
    parent report rides, so the exception travels the real wrapping and
    propagation path. Returns one bool per matching append — True when that
    append raised — in order.
    """
    import src.core.chat_events as chat_events_module
    real = chat_events_module.append_ndjson
    state = {"raised": 0}
    hits: list[bool] = []

    async def flaky(path, data):
        target = str(path)
        if data.get("type") == event_type and any(f"/sessions/{s}/data/" in target for s in session_ids):
            if state["raised"] < times:
                state["raised"] += 1
                hits.append(True)
                if on_raise is not None:
                    on_raise()
                raise OSError(err, os.strerror(err))
            hits.append(False)
        return await real(path, data)

    monkeypatch.setattr(chat_events_module, "append_ndjson", flaky)
    return hits


def inject_run_record_write_fault(
    monkeypatch: pytest.MonkeyPatch,
    *,
    only_finished: bool = True,
    times: int = 1,
    err: int = errno.ENOSPC,
    on_raise: Callable[[], None] | None = None,
) -> list[bool]:
    """The first *times* run-metadata writes raise OSError(*err*); every other
    write — and every later one — writes through the real method.

    Sits at ``RunStore.write_record`` (the one record-mirror write funnel over
    ``atomic_write_text``). ``only_finished`` gates the fault to a finish's
    write, the one that carries ``ended_at``. Returns one bool per matching
    write — True when that write raised — in order.
    """
    from src.core.runs import RunStore
    real = RunStore.write_record
    state = {"raised": 0}
    hits: list[bool] = []

    async def flaky(self, session_id, run):
        if not only_finished or run.ended_at is not None:
            if state["raised"] < times:
                state["raised"] += 1
                hits.append(True)
                if on_raise is not None:
                    on_raise()
                raise OSError(err, os.strerror(err))
            hits.append(False)
        return await real(self, session_id, run)

    monkeypatch.setattr(RunStore, "write_record", flaky)
    return hits


def write_raw_result(run_dir: Path, text: str, *, age_seconds: float = 0.0) -> tuple[Path, datetime]:
    """The run's raw transport log with one successful result event, optionally
    mtime-stamped into the past — the drain's truth source. Returns (path, mtime)."""
    raw = run_dir / RAW_LOG_NAME
    raw.parent.mkdir(parents=True, exist_ok=True)
    raw.write_text(json.dumps({
        "type": "result", "subtype": "success", "is_error": False, "result": text,
    }) + "\n", encoding="utf-8")
    mtime = datetime.fromtimestamp(raw.stat().st_mtime, tz=UTC)
    if age_seconds:
        ts = time.time() - age_seconds
        os.utime(raw, (ts, ts))
        mtime = datetime.fromtimestamp(ts, tz=UTC)
    return raw, mtime


async def _wired_root_manager(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    """(cfg, session_mgr, tree, manager): a root manager task over build_env's home, its
    dispatch wired to the silent-broadcast adapter — the rig prefix the manager-turn tests
    share. Each site unpacks the names it uses."""
    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
    manager = await create_task(tree, parent=None, request_id="root")
    tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
    return cfg, session_mgr, tree, manager


async def _manager_with_worker_child(tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
                                     task_type: str | None = None):
    """(cfg, session_mgr, tree, manager, worker): a manager node with one
    worker child, the shape the end-landing tests report against."""
    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
    manager = await create_task(tree, parent=None, request_id="root")
    worker = await create_task(
        tree, parent=manager.id, request_id="child", profile="worker",
        task=TaskSpec(goal="do the work", task_type=task_type))
    return cfg, session_mgr, tree, manager, worker


class _IdentityTranslator:
    """A translate-only double: the staged raw bytes already carry standard
    event dicts, so a resume's stream translator passes them through."""

    _POST_RESULT_TIMEOUT = 5.0

    def translate_event(self, event: dict) -> list[dict]:
        return [event]


def install_worker_launch_and_resume_backends(monkeypatch: pytest.MonkeyPatch, backends: list) -> list[dict]:
    """Serve worker launcher builds one at a time; translate-only builds (the
    resume follow's fresh translate) get the identity translator instead of
    consuming a launcher double."""
    builds: list[dict] = []
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


async def poll_until(predicate, *, timeout: float = 5.0, poll: float = 0.02, what: str) -> None:
    """Poll a sync predicate to truth without starving the event loop the work
    runs on; fail naming *what* on timeout."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        await asyncio.sleep(poll)
    pytest.fail(f"{what} never settled within {timeout}s")


def child_reports(tree: TaskTreeManager, session_id: str) -> list[dict]:
    return [e for e in tree.fact_history(session_id) if e.get("type") == ET.CHILD_REPORT]


def test_is_out_of_space_error_walks_cause_and_context_chain() -> None:
    """Only ENOSPC/EDQUOT enter the retry, wherever they sit on the chain."""
    from src.core.task_execution import is_out_of_space_error
    enospc = OSError(errno.ENOSPC, "No space left on device")
    edquot = OSError(errno.EDQUOT, "Disk quota exceeded")
    assert is_out_of_space_error(enospc)
    assert is_out_of_space_error(edquot)
    assert is_out_of_space_error(RuntimeError("landing failed")) is False
    assert is_out_of_space_error(PermissionError("raw log locked")) is False
    # Both chain links: an explicit cause and an implicit context.
    wrapped_cause = RuntimeError("wrap")
    wrapped_cause.__cause__ = enospc
    assert is_out_of_space_error(wrapped_cause)
    wrapped_context = RuntimeError("wrap")
    wrapped_context.__context__ = edquot
    assert is_out_of_space_error(wrapped_context)
    # A cycle on the context chain must not spin.
    a: BaseException = RuntimeError("a")
    b: BaseException = RuntimeError("b")
    b.__context__ = a
    a.__context__ = b
    assert is_out_of_space_error(a) is False
    # A cause nested behind a non-OSError wrapper is still found.
    deep: BaseException = ValueError("outer")
    deep.__cause__ = wrapped_cause
    assert is_out_of_space_error(deep)


@pytest.mark.asyncio
async def test_worker_run_finished_enospc_retries_and_lands_without_restart(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """(a) The run_finished write raises ENOSPC, the retry drains the dead run
    from its raw log and lands the end record; the parent gets exactly one
    report and no second process ever starts."""
    from src.core.thinking_state import busy_since
    monkeypatch.setattr(task_execution_module, "RUN_END_LANDING_RETRY_INTERVAL_SECONDS", 0.05)
    cfg, session_mgr, tree, manager, worker = await _manager_with_worker_child(tmp_path, monkeypatch)
    adapter = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
    tree.dispatch.executor = adapter
    patch_instructions_content(monkeypatch)
    worker_builds = install_worker_launch_and_resume_backends(
        monkeypatch, [SpawningScriptedBackend([result_event("work done")])])
    monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, make_pm_build("report consumed"))
    fault_hits = inject_chat_append_fault(
        monkeypatch, event_type=ET.RUN_FINISHED, session_ids={worker.id})

    await tree.dispatch.admit_input(worker.id, event_type=ET.USER, content="Start the task.", actor="user")
    decision = await tree.dispatch.dispatch_pending(worker.id)
    run_id = decision["run_id"]
    await wait_for_terminal_run(tree, worker.id, run_id)
    assert fault_hits == [True, False]  # one failed write; the retry's write landed
    # The worker header timer the failed finish could not close is closed by
    # the hook-1 handover (the process is dead): the node shows idle again.
    await poll_until(lambda: busy_since(worker.id) is None, what="the worker header timer")
    # The retried landing delivered the success-based report exactly once, and
    # the parent's report turn settles without a restart. The report rides the
    # retry task's async delivery chain, so wait for the report itself: an
    # idle-parent poll can win the race against the not-yet-appended report.
    await poll_until(lambda: len(child_reports(tree, manager.id)) == 1, what="the retried landing's report")
    await _settle_parent(tree, manager, timeout=5.0, poll=0.02)
    reports = child_reports(tree, manager.id)
    assert [r.get("outcome") for r in reports] == ["completed"]
    assert adapter._landing_retries == {}  # a clean round ended the retry task
    assert len(worker_builds) == 1  # one process: the retry drained, never relaunched


@pytest.mark.asyncio
async def test_manager_turn_master_done_enospc_retries_through_master_queue(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """(b) The manager turn's MASTER_DONE write raises ENOSPC; the retry's node
    reconcile pass re-attaches the dead turn through the master queue (the kept
    future's done-callback releases the follow pair) and lands the end record
    with the raw log's last write time."""
    monkeypatch.setattr(task_execution_module, "RUN_END_LANDING_RETRY_INTERVAL_SECONDS", 0.05)
    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
    manager = await create_task(tree, parent=None, request_id="root")
    adapter = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
    tree.dispatch.executor = adapter
    monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, make_pm_build("turn reply"))
    patch_instructions_content(monkeypatch)
    stub_credentials({"charliebot": {"access_key": "op-secret"}})
    await tree.dispatch.admit_input(manager.id, event_type=ET.USER, content="Take off.", actor="user")
    decision = await tree.dispatch.dispatch_pending(manager.id)
    run_id = decision["run_id"]
    run_dir = tree.runs.run_dir(manager.id, run_id)
    # The turn's own MASTER_DONE write fails out of space; the drain reads the
    # raw log the live turn never wrote (the scripted double writes none), so
    # the fault itself stages it — the retry's drain outruns any later write.
    _staged: list[tuple[Path, object]] = []

    def _stage_drain_raw_log() -> None:
        _staged.append(write_raw_result(run_dir, "drained turn result"))

    fault_hits = inject_chat_append_fault(
        monkeypatch, event_type=ET.MASTER_DONE, session_ids={manager.id},
        on_raise=_stage_drain_raw_log)
    run, outcome = await wait_for_terminal_run(tree, manager.id, run_id)
    assert fault_hits == [True, False]  # the live write failed; the drain's landed
    assert len(_staged) == 1
    raw_mtime = _staged[0][1]
    assert outcome == "success"
    assert run.ended_at is not None and abs(run.ended_at - raw_mtime) < timedelta(seconds=1)
    # The follow pair released only when the master-queue future resolved.
    await poll_until(lambda: (manager.id, run_id) not in adapter._resume_follows,
               what="the manager-turn follow pair release")
    assert adapter._landing_retries == {}


@pytest.mark.asyncio
async def test_parent_report_enospc_retry_delivers_report_once(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """(c)+(i)+(k) A successful run's delivery write raises ENOSPC: the end
    record is already on disk, the retry delivers the success-based report
    exactly once (never a failed report), and the run's live ended_at survives
    the retry untouched."""
    monkeypatch.setattr(task_execution_module, "RUN_END_LANDING_RETRY_INTERVAL_SECONDS", 0.05)
    cfg, session_mgr, tree, manager, worker = await _manager_with_worker_child(tmp_path, monkeypatch)
    adapter = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
    tree.dispatch.executor = adapter
    patch_instructions_content(monkeypatch)
    install_worker_launch_and_resume_backends(
        monkeypatch, [SpawningScriptedBackend([result_event("work done")])])
    monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, make_pm_build("report consumed"))
    fault_hits = inject_chat_append_fault(
        monkeypatch, event_type=ET.CHILD_REPORT, session_ids={manager.id})

    await tree.dispatch.admit_input(worker.id, event_type=ET.USER, content="Start the task.", actor="user")
    decision = await tree.dispatch.dispatch_pending(worker.id)
    run_id = decision["run_id"]
    run, outcome = await wait_for_terminal_run(tree, worker.id, run_id)
    assert outcome == "success"
    # The retry's re-delivery is asynchronous with the live finish: wait for
    # the one report before naming the appends and settling the parent.
    await poll_until(lambda: len(child_reports(tree, manager.id)) == 1, what="the retried delivery's report")
    assert fault_hits == [True, False]  # the live delivery failed; the retry's landed
    ended_at = run.ended_at
    assert ended_at is not None
    await _settle_parent(tree, manager, timeout=5.0, poll=0.02)
    reports = child_reports(tree, manager.id)
    assert [r.get("outcome") for r in reports] == ["completed"]  # success-based, once
    fresh = await tree.runs.get_run(worker.id, run_id)
    assert fresh is not None and fresh.ended_at == ended_at  # (k) the retry kept it
    assert adapter._landing_retries == {}


@pytest.mark.asyncio
async def test_drain_ended_at_is_raw_log_last_write_live_exit_keeps_write_time(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """(d) A drained run's ended_at is the raw log's last write time; a live
    exit's ended_at stays the observed-exit write time."""
    from src.core.task_recovery import reconcile_task_tree
    cfg, session_mgr, tree, manager, worker = await _manager_with_worker_child(tmp_path, monkeypatch)
    adapter = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
    tree.dispatch.executor = adapter
    patch_instructions_content(monkeypatch)
    install_worker_launch_and_resume_backends(
        monkeypatch, [SpawningScriptedBackend([result_event("drained work")])])

    # Drain: a launched run whose process died unseen, raw log stamped an hour ago.
    run_id = "run-drained"
    await tree.runs.register_run(
        RunRecord(id=run_id, session_id=worker.id, kind="work", backend="fake", model="fake-model"))
    await tree.dispatch.admit_input(worker.id, event_type=ET.USER, content="Start the task.", actor="user")
    await tree.dispatch.claim_input_batch_locked(worker.id, run_id)
    await tree.runs.record_launch(worker.id, run_id, pid=424901, pid_start="1-424000")
    _raw, raw_mtime = write_raw_result(
        tree.runs.run_dir(worker.id, run_id), "drained result", age_seconds=3600)
    await reconcile_task_tree(cfg, tree, adapter)
    run, outcome = await wait_for_terminal_run(tree, worker.id, run_id)
    assert outcome == "success"
    assert run.exit_code == 0
    assert run.ended_at is not None and abs(run.ended_at - raw_mtime) < timedelta(seconds=1)

    # Live exit: the observed-exit write time, never a staged raw-log mtime.
    # A fresh child node: the drained node's trailing dispatch would race a
    # second input admitted on it.
    worker2 = await create_task(
        tree, parent=manager.id, request_id="child-2", profile="worker",
        task=TaskSpec(goal="more work"))
    install_worker_launch_and_resume_backends(
        monkeypatch, [SpawningScriptedBackend([result_event("live work")])])
    await tree.dispatch.admit_input(worker2.id, event_type=ET.USER, content="Start.", actor="user")
    decision = await tree.dispatch.dispatch_pending(worker2.id)
    live_run, live_outcome = await wait_for_terminal_run(tree, worker2.id, decision["run_id"])
    assert live_outcome == "success"
    assert live_run.ended_at is not None and live_run.ended_at > raw_mtime


@pytest.mark.asyncio
async def test_disk_headroom_precheck_withholds_worker_run_but_not_manager_turn(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """(e) Below the threshold a worker-class run stays queued with one blocked
    report; a manager turn still launches; threshold 0 skips the check."""
    cfg, session_mgr, tree, manager, worker = await _manager_with_worker_child(tmp_path, monkeypatch)
    cfg.server.min_free_disk_gib = 10 ** 9  # no filesystem holds this much
    adapter = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
    tree.dispatch.executor = adapter
    patch_instructions_content(monkeypatch)
    worker_builds = install_worker_launch_and_resume_backends(
        monkeypatch, [SpawningScriptedBackend([result_event("work done")])])
    monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, make_pm_build("report consumed"))

    await tree.dispatch.admit_input(worker.id, event_type=ET.USER, content="Start the task.", actor="user")
    decision = await tree.dispatch.dispatch_pending(worker.id)
    run_id = decision["run_id"]

    def _withheld_reason() -> str | None:
        events = tree.events.load_events(worker.id)
        for event in events:
            if event.get("type") == ET.RUN_LAUNCH_WITHHELD:
                return str(event.get("reason"))
        return None

    # The launch is fire-and-forget: poll for the withheld verdict's durable
    # event, then re-read the queued run it left behind.
    await poll_until(lambda: _withheld_reason() is not None, what="the disk-headroom withheld event")
    reason = _withheld_reason() or ""
    assert "disk free" in reason and "below 1000000000 GiB" in reason
    assert str(cfg.charliebot_home) in reason or str(Path(cfg.paths.worktree_dir)) in reason
    run = await tree.runs.get_run(worker.id, run_id)
    assert run is not None and run.pid is None  # no process, stays queued
    assert tree.runs.terminal_outcome(tree.runs.load_events_sync(worker.id), run_id) is None
    assert worker_builds == []
    await poll_until(lambda: len(child_reports(tree, manager.id)) == 1, what="the blocked report")
    assert [r.get("outcome") for r in child_reports(tree, manager.id)] == ["blocked"]

    # A manager turn launches under the same threshold: the blocked report's
    # own wake started one (worker-class runs only are checked).
    await _settle_parent(tree, manager, timeout=5.0, poll=0.02)
    manager_events = tree.runs.load_events_sync(manager.id)
    turns = [r for r in tree.runs.list_run_records_sync(manager.id) if r.kind == "manager_turn"]
    assert len(turns) == 1
    assert tree.runs.terminal_outcome(manager_events, turns[0].id) == "success"
    # ...and threshold 0 skips the check, so the queued worker launches.
    cfg.server.min_free_disk_gib = 0
    decision = await tree.dispatch.dispatch_pending(worker.id)
    run, outcome = await wait_for_terminal_run(tree, worker.id, decision["run_id"])
    assert outcome == "success"


@pytest.mark.asyncio
async def test_non_space_end_failure_lands_immediately_and_starts_no_retry(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """(j) A non-out-of-space exception in the end path lands the run
    immediately (as today) and starts no retry; a success fact that hits
    OSError(EIO) during delivery sends no failed report."""
    monkeypatch.setattr(task_execution_module, "RUN_END_LANDING_RETRY_INTERVAL_SECONDS", 0.05)
    cfg, session_mgr, tree, manager, worker = await _manager_with_worker_child(tmp_path, monkeypatch)
    adapter = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
    tree.dispatch.executor = adapter
    patch_instructions_content(monkeypatch)
    install_worker_launch_and_resume_backends(
        monkeypatch,
        [SpawningScriptedBackend([result_event("work done")]),
         SpawningScriptedBackend([result_event("more work done")])])
    monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, make_pm_build("report consumed"))

    # First half: the run_finished write raises PermissionError (not space).
    inject_chat_append_fault(
        monkeypatch, event_type=ET.RUN_FINISHED, session_ids={worker.id}, err=errno.EACCES)
    await tree.dispatch.admit_input(worker.id, event_type=ET.USER, content="Start the task.", actor="user")
    decision = await tree.dispatch.dispatch_pending(worker.id)
    run_id = decision["run_id"]
    _run, outcome = await wait_for_terminal_run(tree, worker.id, run_id)
    assert outcome == "failed"  # landed immediately by the existing path
    assert adapter._landing_retries == {}  # no retry for a non-space error
    await _settle_parent(tree, manager, timeout=5.0, poll=0.02)
    assert [r.get("outcome") for r in child_reports(tree, manager.id)] == ["failed"]

    # Second half: a success fact that hits EIO during delivery — the durable
    # outcome governs the delivery, so no contradicting failed report, and no
    # retry (EIO does not clear with freed space).
    fresh = await tree.runs.get_run(worker.id, decision["run_id"])
    assert fresh is not None and fresh.id == run_id
    inject_chat_append_fault(
        monkeypatch, event_type=ET.CHILD_REPORT, session_ids={manager.id},
        times=50, err=errno.EIO)
    await tree.dispatch.admit_input(worker.id, event_type=ET.USER, content="More work.", actor="user")
    decision = await tree.dispatch.dispatch_pending(worker.id)
    second_id = decision["run_id"]
    _second_run, second_outcome = await wait_for_terminal_run(tree, worker.id, second_id)
    assert second_outcome == "success"
    assert adapter._landing_retries == {}
    await poll_until(lambda: len(child_reports(tree, manager.id)) >= 1, what="the retried delivery")
    outcomes = [r.get("outcome") for r in child_reports(tree, manager.id)]
    assert "failed" not in outcomes[1:], outcomes  # the second run sent no failed report


@pytest.mark.asyncio
async def test_repeat_finish_fills_only_empty_end_metadata(tmp_path: Path,
                                                           monkeypatch: pytest.MonkeyPatch) -> None:
    """finish_run's repeat path fills empty ended_at/exit_code from the passed
    values and never moves values already written."""
    _cfg, _session_mgr, tree = build_env(tmp_path, monkeypatch)
    worker = await create_task(
        tree, parent=None, request_id="w", profile="worker", task=TaskSpec(goal="g"))
    run_id = "run-half"
    await tree.runs.register_run(RunRecord(id=run_id, session_id=worker.id, kind="work"))
    # First finish with an empty-ended_at repeat (the half-written record's
    # shape): the fact lands, the metadata write is lost.
    await tree.dispatch.finish_run(worker.id, run_id, outcome="success", exit_code=0)
    meta_path = tree.runs.metadata_path(worker.id, run_id)
    record = json.loads(meta_path.read_text(encoding="utf-8"))
    record["ended_at"] = None
    record["exit_code"] = None
    meta_path.write_text(json.dumps(record), encoding="utf-8")
    tree.runs.records_generation += 1

    ended = datetime.now(UTC) - timedelta(minutes=5)
    await tree.dispatch.finish_run(worker.id, run_id, outcome="failed", exit_code=-1, ended_at=ended)
    run = await tree.runs.get_run(worker.id, run_id)
    assert run is not None
    # The first outcome never moves; the empty fields take the passed values.
    assert tree.runs.terminal_outcome(tree.runs.load_events_sync(worker.id), run_id) == "success"
    assert run.ended_at is not None and abs(run.ended_at - ended) < timedelta(seconds=1)
    assert run.exit_code == -1
    # A repeat over complete metadata changes nothing, whatever it names.
    other = ended + timedelta(minutes=1)
    await tree.dispatch.finish_run(worker.id, run_id, outcome="failed", exit_code=-2, ended_at=other)
    run = await tree.runs.get_run(worker.id, run_id)
    assert run is not None
    assert run.ended_at == run.ended_at and run.ended_at is not None
    assert abs(run.ended_at - ended) < timedelta(seconds=1)
    assert run.exit_code == -1
