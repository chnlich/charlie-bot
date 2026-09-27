"""Execution-adapter tests: v2 Runs bound to the existing master/worker harnesses.

Covers the required behavior wiring: durable dispatch to actual execution (one
Run, exact batch, launched identity persisted before its credential works),
delegate as task/run with stable replay identity, queued retry launches,
failure requiring explicit retry, first-terminal-fact-wins, worker work/review
delivery through the same task, and real landing verification.
"""

from __future__ import annotations

import asyncio
import json
import subprocess
from collections.abc import AsyncIterator, Awaitable, Callable
from pathlib import Path

import pytest
from conftest import (
    BUILD_BACKEND_PATCH_TARGET,
    WORKER_BUILD_BACKEND_PATCH_TARGET,
    backend_option,
    create_task,
    patch_instructions_content,
    stub_credentials,
)
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.core import event_types as ET
from src.core.models import BackendOption, PatchSessionTaskRequest, RunRecord, TaskSpec
from src.core.run_token import CallerIdentity
from src.core.sessions import SessionManager
from src.core.task_sessions import TaskTreeManager

OPERATOR = {"Authorization": "Bearer op-secret"}
OP_CALLER = CallerIdentity(kind="operator")


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


# ---------------------------------------------------------------------------
# Serialized turns: inputs admitted during an active run dispatch on its finish


# ---------------------------------------------------------------------------
# Delegate: one worker child task with its first Run, stable replay identity


# ---------------------------------------------------------------------------
# Retry: queued runs, stops, and the explicit-retry gate
# ---------------------------------------------------------------------------


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
    tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
    pm_builds = []
    monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, make_pm_build("manager turn", pm_builds))
    patch_instructions_content(monkeypatch)
    stub_credentials({"charliebot": {"access_key": "op-secret"}})
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
    deadline = asyncio.get_event_loop().time() + 30
    while asyncio.get_event_loop().time() < deadline:
        pm_events = tree.runs.load_events_sync(manager.id)
        active = [r for r in tree.runs.list_run_records_sync(manager.id)
                  if tree.runs.terminal_outcome(pm_events, r.id) is None]
        if not tree.dispatch.pending_inputs(manager.id) and not active:
            break
        await asyncio.sleep(0.2)
    else:
        pytest.fail("the parent's report turns never settled")
    assert pm_builds, "the delivered reports never triggered a parent manager turn"
    assert tree.task_state(manager.id) == "open"


def _advance_origin_from_second_clone(tmp_path: Path, origin: Path, filename: str) -> str:
    """Push one commit to the bare origin from a second clone; returns the new tip."""
    second = tmp_path / "second-clone"
    subprocess.run(["git", "clone", "-q", str(origin), str(second)], check=True)
    git(second, "config", "user.email", "t@example.com")
    git(second, "config", "user.name", "t")
    (second / filename).write_text("advance\n")
    git(second, "add", "-A")
    git(second, "commit", "-q", "-m", f"advance origin via {filename}")
    git(second, "push", "-q", "origin", "main")
    return git(second, "rev-parse", "refs/heads/main")


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


async def _wait_for_parent_report(tree: TaskTreeManager, parent_id: str, timeout: float = 15.0) -> dict:
    """Poll the parent's fact history until a delivered child_report lands."""
    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        reports = [e for e in tree.events.load_events(parent_id) if e.get("type") == ET.CHILD_REPORT]
        if reports:
            return reports[-1]
        await asyncio.sleep(0.1)
    pytest.fail(f"the parent {parent_id} never received a failure report within {timeout}s")


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
    tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
    pm_builds = []
    monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, make_pm_build("manager turn", pm_builds))
    patch_instructions_content(monkeypatch)
    stub_credentials({"charliebot": {"access_key": "op-secret"}})
    # The work-run launch re-judges the nearest-user authorization gate; the
    # manager carries the real user takeoff message the delegation rode in on.
    await tree.dispatch.admit_input(
        manager.id, event_type=ET.USER,
        content="Take off and implement the marker file.", actor="user")

    # Origin gains a commit from a second clone while the fixture repo's local
    # main stays put: local main is strictly behind origin/main.
    origin_tip = _advance_origin_from_second_clone(tmp_path, origin, "ahead.txt")
    local_main = git(repo, "rev-parse", "main")
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
    assert git(work_wt, "rev-parse", "HEAD") == origin_tip
    assert git(repo, "rev-parse", "main") == local_main

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
    deadline = asyncio.get_event_loop().time() + 30
    while asyncio.get_event_loop().time() < deadline:
        pm_events = tree.runs.load_events_sync(manager.id)
        active = [r for r in tree.runs.list_run_records_sync(manager.id)
                  if tree.runs.terminal_outcome(pm_events, r.id) is None]
        if not tree.dispatch.pending_inputs(manager.id) and not active:
            break
        await asyncio.sleep(0.2)
    else:
        pytest.fail("the parent's report turn never settled")


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
    tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
    pm_builds = []
    monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, make_pm_build("manager turn", pm_builds))
    patch_instructions_content(monkeypatch)
    stub_credentials({"charliebot": {"access_key": "op-secret"}})
    await tree.dispatch.admit_input(
        manager.id, event_type=ET.USER,
        content="Take off and implement the marker file.", actor="user")

    # One unpushed commit on the fixture repo's local main: origin does not
    # have it, so the base check fails closed.
    (repo / "local_only.txt").write_text("local work\n")
    git(repo, "add", "-A")
    git(repo, "-c", "user.email=t@example.com", "-c", "user.name=t", "commit", "-q", "-m", "unpushed local commit")
    local_tip = git(repo, "rev-parse", "main")

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


def _task_spec(tree: TaskTreeManager, spec: dict):
    from src.core.models import TaskType
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


# ---------------------------------------------------------------------------
# The context stage: one assembler at the actual launch boundary, snapshot
# evidence, native continuation, and preparation failures
# ---------------------------------------------------------------------------


def _manager_backend(monkeypatch: pytest.MonkeyPatch, tree, cfg, session_mgr, *, events: list[dict]) -> tuple[list[SpawningScriptedBackend], list[dict]]:
    tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
    backend = SpawningScriptedBackend(events)
    builds = install_backends(monkeypatch, [backend], BUILD_BACKEND_PATCH_TARGET)
    return [backend], builds


@pytest.mark.asyncio
async def test_missing_rule_fails_before_launch_and_leaves_input_unconsumed(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
    manager = await create_task(tree, parent=None, request_id="root")
    await tree.patch_task(manager.id, PatchSessionTaskRequest(node_prompt="needed rule"),
                          caller=OP_CALLER)
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


# ---------------------------------------------------------------------------
# Launch failure past admission: a durable failed run, its error evidence, and
# the parent report a failed process would produce
