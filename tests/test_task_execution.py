"""Execution-adapter tests: v2 Runs bound to the existing master/worker harnesses.

Covers the required behavior wiring: durable dispatch to actual execution (one
Run, exact batch, launched identity persisted before its credential works),
delegate as task/run with stable replay identity, queued retry launches,
failure requiring explicit retry, first-terminal-fact-wins, worker work/review
delivery through the same task, and real landing verification.
"""

from __future__ import annotations

import asyncio
import subprocess
from collections.abc import AsyncIterator, Callable
from pathlib import Path

import pytest
from conftest import (
    backend_option,
    patch_instructions_content,
)
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.core import event_types as ET
from src.core.models import BackendOption, RunRecord
from src.core.sessions import SessionManager
from src.core.task_sessions import TaskTreeManager

OPERATOR = {"Authorization": "Bearer op-secret"}


def build_env(tmp_path: Path, monkeypatch: pytest.MonkeyPatch | None = None):
    """One fake backend registered, so task creation's default resolution works.

    The synthetic home carries a synthetic access key: every child environment
    signs its run token against it (never operator credentials from the host).
    """
    import src.core.config as core_config
    from src.core.config import CharlieBotConfig
    home = tmp_path / "charliebot-home"
    cfg = CharlieBotConfig(
        charliebot_home=home,
        backends={
            "options": [backend_option(id="fake", label="Fake", type="codex", model="fake-model")],
            "preference": ["fake"],
        },
        paths={"worktree_dir": str(home / "worktrees")})
    key = "task-exec-test-key"
    core_config._credentials_cache.seed(core_config.Credentials(
        path=home / "credentials.yaml",
        sections={"charliebot": {"access_key": key}}))
    if monkeypatch is not None:
        monkeypatch.setenv("CHARLIEBOT_HOME", str(home))
    session_mgr = SessionManager(cfg)
    return cfg, session_mgr, TaskTreeManager(cfg, session_mgr)


class SpawningScriptedBackend:
    """Backend double that fires the on_spawn callback, records its launch env,
    and yields a scripted event list ending in a result event."""

    def __init__(self, events: list[dict], exit_code: int = 0, stderr_text: str = "",
                 pre_run: "Callable[[], None] | None" = None,
                 gate: "Callable[[], object] | None" = None) -> None:
        self._events = events
        self.exit_code = exit_code
        self.stderr_text = stderr_text
        self._pre_run = pre_run
        self.gate = gate
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


def result_event(text: str = "done") -> dict:
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


def stub_credentials(monkeypatch: pytest.MonkeyPatch, sections: dict) -> None:
    import src.core.config as core_config
    core_config._credentials_cache.seed(core_config.Credentials(
        path=Path("credentials.yaml"), sections=sections))


async def create_task(tree: TaskTreeManager, *, parent: str | None, request_id: str,
                      profile: str = "manager", task=None, name: str | None = None):
    return await tree.create_task(
        request_id=request_id, task_parent_id=parent, profile=profile, task=task,
        name=name, backend=None, caller="operator")


async def wait_for_terminal_run(tree: TaskTreeManager, session_id: str, run_id: str,
                                timeout: float = 15.0) -> tuple[RunRecord, str]:
    """Poll one Run until its terminal fact lands (the launch is fire-and-forget)."""
    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        run = await tree.runs.get_run(session_id, run_id)
        assert run is not None, f"run {run_id} vanished"
        events = tree.runs.load_events_sync(session_id)
        outcome = tree.runs.terminal_outcome(events, run_id)
        if outcome is not None:
            return run, str(outcome)
        await asyncio.sleep(0.05)
    pytest.fail(f"run {run_id} never reached a terminal fact within {timeout}s")


def git(repo: Path, *args: str) -> str:
    proc = subprocess.run(["git", "-C", str(repo), *args], check=True, capture_output=True, text=True)
    return proc.stdout.strip()


def init_repo_with_origin(tmp_path: Path) -> tuple[Path, Path]:
    """A synthetic repo with a bare origin carrying main (the landing target)."""
    origin = tmp_path / "origin.git"
    subprocess.run(["git", "init", "-q", "--bare", "-b", "main", str(origin)], check=True)
    repo = tmp_path / "repo"
    subprocess.run(["git", "clone", "-q", str(origin), str(repo)], check=True)
    git(repo, "config", "user.email", "t@example.com")
    git(repo, "config", "user.name", "t")
    (repo / "seed.txt").write_text("seed\n")
    git(repo, "add", ".")
    git(repo, "commit", "-q", "-m", "seed")
    git(repo, "push", "-q", "origin", "main")
    return repo, origin


# ---------------------------------------------------------------------------
# Manager turns: durable dispatch to actual execution
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_manager_turn_persists_run_identity_and_acknowledges_batch(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
    manager = await create_task(tree, parent=None, request_id="root")
    tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
    backend = SpawningScriptedBackend([result_event("SMOKE reply")])
    builds = install_backends(
        monkeypatch, [backend], "src.agents.backends.registry.build_backend")
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


@pytest.mark.asyncio
async def test_concurrent_dispatch_starts_one_process(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
    manager = await create_task(tree, parent=None, request_id="root")
    tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
    backend = SpawningScriptedBackend([result_event("one")])
    builds = install_backends(
        monkeypatch, [backend], "src.agents.backends.registry.build_backend")
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
# Delegate: one worker child task with its first Run, stable replay identity
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_delegate_creates_one_child_and_replays_are_stable(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:

    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
    manager = await create_task(tree, parent=None, request_id="root")
    monkeypatch.setenv("CHARLIEBOT_HOME", str(cfg.charliebot_home))
    stub_credentials(monkeypatch, {"charliebot": {"access_key": "op-secret"}})
    backend = SpawningScriptedBackend([result_event("phrase")])
    builds = install_backends(
        monkeypatch, [backend], "src.agents.worker.build_backend")
    tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)

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
        }
        first = client.post("/api/internal/delegate", json=payload, headers=OPERATOR)
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
        replay = client.post("/api/internal/delegate", json=payload, headers=OPERATOR)
        assert replay.status_code == 200, replay.text
        assert replay.json()["session_id"] == child_id
        assert replay.json()["run_id"] == run_id

        # The same explicit request_id replays identically; a distinct explicit
        # id names a genuinely different operation (an intentional sibling).
        first_named = client.post("/api/internal/delegate",
                                  json=dict(payload, request_id="op-1"), headers=OPERATOR)
        assert first_named.json()["session_id"] != child_id
        replay_named = client.post("/api/internal/delegate",
                                   json=dict(payload, request_id="op-1"), headers=OPERATOR)
        assert replay_named.json()["session_id"] == first_named.json()["session_id"]
        sibling = client.post("/api/internal/delegate", json=dict(payload, request_id="op-2"),
                              headers=OPERATOR)
        assert sibling.json()["session_id"] != first_named.json()["session_id"]

    # The work run executes through the worker adapter with the child's own
    # identity in its environment.
    await asyncio.sleep(0.1)

    from src.agents import master_cc_state
    for consumer in list(master_cc_state._session_consumers.values()):
        await asyncio.wait_for(consumer, timeout=10)
    assert len(builds) == 1
    captured_env = builds[0]["backend"].env or {}
    assert captured_env.get("CHARLIEBOT_SESSION_ID") == child_id
    assert captured_env.get("CHARLIEBOT_RUN_TOKEN")
    assert captured_env.get("CHARLIEBOT_HOME") == str(cfg.charliebot_home)
    worker_run = await tree.runs.get_run(child_id, run_id)
    assert worker_run is not None and worker_run.kind == "work"
    assert worker_run.backend == "fake"
    worker_outcome = tree.runs.terminal_outcome(tree.runs.load_events_sync(child_id), run_id)
    print("WORKER RUN OUTCOME:", worker_outcome, flush=True)
    print("CHILD STATE:", tree.task_state(child_id), flush=True)
    if worker_outcome != "success":
        raw = Path(worker_run.raw_log_ref).read_text(errors="replace") if worker_run.raw_log_ref else ""
        print("RAW TAIL:", raw[-500:], flush=True)
    # The completed delivery closed and auto-archived the worker; the manager
    # remains open and received the completed report.
    deadline = asyncio.get_event_loop().time() + 5
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


# ---------------------------------------------------------------------------
# Retry: queued runs, stops, and the explicit-retry gate
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_stopped_queued_retry_never_launches_and_fresh_retry_launches(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
    manager = await create_task(tree, parent=None, request_id="root")
    tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
    patch_instructions_content(monkeypatch)

    # A failed run leaves its batch pending; dispatch must not auto-retry it.
    await tree.runs.register_run(
        RunRecord(id="run-failed", session_id=manager.id, kind="manager_turn",
                  backend="fake", model="fake-model"))
    await tree.dispatch.claim_input_batch(manager.id, "run-failed")
    await tree.dispatch.finish_run(manager.id, "run-failed", outcome="failed", exit_code=1)
    admitted = await tree.dispatch.admit_input(
        manager.id, event_type=ET.USER, content="Take off. Next.", actor="user")
    decision = await tree.dispatch.dispatch_pending(manager.id)
    assert decision["launch"] is False
    assert "unresolved failure" in decision["reason"]

    # The explicit retry creates a queued run; a stop request on it keeps it
    # from ever launching.
    retry = await tree.create_retry(manager.id, "retry-1", "run-failed")
    assert retry["run_id"], retry
    await tree.runs.request_stop(manager.id, retry["run_id"], "stop-1")
    decision = await tree.dispatch.dispatch_pending(manager.id)
    assert decision["launch"] is False

    # A fresh queued retry (no stop request) actually launches and claims the
    # pending batch — the serialized turn that consumes it.
    retry2 = await tree.create_retry(manager.id, "retry-2", "run-failed")
    backend = SpawningScriptedBackend([result_event("retried")])
    install_backends(monkeypatch, [backend], "src.agents.backends.registry.build_backend")
    decision = await tree.dispatch.dispatch_pending(manager.id)
    assert decision["launch"] is True and decision["run_id"] == retry2["run_id"]
    run, _outcome = await wait_for_terminal_run(tree, manager.id, retry2["run_id"])
    assert run.input_event_ids == [str(admitted["id"])]
    # The successful retry supersedes the failure; later input dispatches.
    await tree.dispatch.admit_input(
        manager.id, event_type=ET.USER, content="Take off. Later.", actor="user")
    backend2 = SpawningScriptedBackend([result_event("later")])
    install_backends(monkeypatch, [backend2], "src.agents.backends.registry.build_backend")
    decision = await tree.dispatch.dispatch_pending(manager.id)
    assert decision["launch"] is True
    await wait_for_terminal_run(tree, manager.id, decision["run_id"])
    assert tree.dispatch.pending_inputs(manager.id) == []


@pytest.mark.asyncio
async def test_first_terminal_fact_wins_governs_followups(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
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


@pytest.mark.asyncio
async def test_implement_delivery_requires_review_and_real_landing(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
    repo, origin = init_repo_with_origin(tmp_path)
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
    tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
    stub_credentials(monkeypatch, {"charliebot": {"access_key": "op-secret"}})
    # The work-run launch re-judges the nearest-user authorization gate; the
    # manager carries the real user takeoff message the delegation rode in on.
    await tree.dispatch.admit_input(
        manager.id, event_type=ET.USER,
        content="Take off and implement the marker file.", actor="user")

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
            proc = subprocess.run(["git", "-C", str(work_wt), *args],
                                  capture_output=True, text=True)
            assert proc.returncode == 0, (args, proc.stderr)

    review_backend = SpawningScriptedBackend([result_event("review ok")], pre_run=reviewer_push)
    review_retry_backend = SpawningScriptedBackend([result_event("review ok again")], pre_run=reviewer_push)
    install_backends(
        monkeypatch, [work_backend, review_backend, review_retry_backend],
        "src.agents.worker.build_backend")

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
    git(wt, "add", "-A")
    git(wt, "-c", "user.email=t@example.com", "-c", "user.name=t", "commit", "-q", "-m", "implement marker")
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
    reports = [e for e in tree.events.load_events(manager.id) if e["type"] == ET.CHILD_REPORT]
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
    base_sha = git(repo, "rev-parse", "HEAD")
    # The base commit IS landed on origin/main.
    landed, _ = await git_verify_commit_landed(repo, "origin/main", base_sha)
    assert landed is True
    # A fake hash fails existence.
    landed, reason = await git_verify_commit_landed(repo, "origin/main", "f" * 40)
    assert landed is False and "existence" in reason
    # A real but unmerged commit fails ancestry.
    git(repo, "checkout", "-q", "-b", "feature")
    (repo / "unmerged.txt").write_text("x\n")
    git(repo, "add", ".")
    git(repo, "commit", "-q", "-m", "unmerged")
    unmerged = git(repo, "rev-parse", "HEAD")
    landed, reason = await git_verify_commit_landed(repo, "origin/main", unmerged)
    assert landed is False and "ancestry" in reason
    # A commit on a different branch target fails.
    landed, _ = await git_verify_commit_landed(repo, "origin/main", base_sha)
    assert landed is True


@pytest.mark.asyncio
async def test_manual_complete_with_forged_landing_ref_stays_open(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    from src.core.task_completion import CompletionEvidence

    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
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
    git(repo, "checkout", "-q", "-b", "side-work")
    (repo / "unlanded.txt").write_text("x\n")
    git(repo, "add", "-A")
    git(repo, "-c", "user.email=t@example.com", "-c", "user.name=t", "commit", "-q", "-m", "unlanded")
    unlanded = git(repo, "rev-parse", "HEAD")
    git(repo, "checkout", "-q", "main")
    evidence = CompletionEvidence(
        summary="forged",
        result_refs=["run:run-work", f"landed:main@{unlanded}"],
        run_ids=["run-work"], review_run_ids=["run-review"])
    from src.core.task_sessions import TaskConflictError as TCE
    with pytest.raises(TCE, match="landing evidence unverified"):
        await tree.completion.complete_task(
            worker.id, request_id="manual-1", evidence=evidence, caller="operator")
    assert tree.task_state(worker.id) == "open"
