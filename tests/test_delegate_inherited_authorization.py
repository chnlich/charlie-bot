"""Inherited authorization on the real v2 entry points.

The accepted v2 policy is one central owner: the nearest-real-user-ancestor
gate (``TaskTreeManager.check_task_authorization``), applied at delegation and
re-judged at the actual launch. A sub-task manager under an authorized project
delegates through the real /api/internal/delegate route without carrying its
own take-off; a local user instruction shadows the ancestor; agent, cron, and
report texts never mint authorization; verify stays exempt (read-only) on the
route and at launch; v1 semantics are untouched. Every scenario here runs the
actual HTTP route against a synthetic instance with a scripted backend.
"""

from __future__ import annotations

import asyncio
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest

from src.core import event_types as ET
from src.core.models import RunRecord, TaskSpec
from src.core.run_token import RunTokenClaims, sign_run_token
from tests.test_task_execution import (
    OPERATOR,
    SpawningScriptedBackend,
    _adapter_with_silent_broadcast,
    build_env,
    init_repo_with_origin,
    install_backends,
    result_event,
    stub_credentials,
)


async def make_tree(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    """One synthetic instance: root manager + child manager under it, plus the
    API client and the scripted executor, with no user input anywhere yet."""
    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
    from conftest import patch_instructions_content
    patch_instructions_content(monkeypatch)
    stub_credentials(monkeypatch, {"charliebot": {"access_key": "op-secret"}})
    monkeypatch.setenv("CHARLIEBOT_HOME", str(cfg.charliebot_home))
    tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
    # The spawn-style routes resolve backends through the config owner directly.
    from src.api import internal as internal_api
    monkeypatch.setattr(internal_api, "get_config", lambda: cfg)
    root = await tree.create_task(
        request_id="root", task_parent_id=None, profile="manager",
        task=TaskSpec(goal="project"), name="Project", backend=None, caller="operator")
    child = await tree.create_task(
        request_id="child", task_parent_id=root.id, profile="manager",
        task=TaskSpec(goal="feature"), name="Feature", backend=None, caller="operator")
    return cfg, session_mgr, tree, root, child


def delegate_payload(session_id: str, repo: Path, *, task_type: str = "quick-edit") -> dict:
    return {
        "session_id": session_id,
        "description": "## Goal\n\nfix the thing\n",
        "task_type": task_type,
        "keep_worktree": False,
        "repo_path": None if task_type == "verify" else str(repo),
        "base_branch": None if task_type == "verify" else "main",
    }


@pytest.fixture()
def repo(tmp_path: Path) -> Path:
    r, _origin = init_repo_with_origin(tmp_path / "authz-repo")
    return r


@pytest.mark.asyncio
async def test_authorized_ancestor_delegates_without_a_local_takeoff(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
    """The real route from a child manager with NO local user message and an
    authorized ancestor creates one leaf and one launched Run; a replay returns
    the same child/Run and never spawns a second process."""
    cfg, session_mgr, tree, root, child = await make_tree(tmp_path, monkeypatch)
    builds = install_backends(
        monkeypatch, [SpawningScriptedBackend([result_event("leaf done")])],
        "src.agents.worker.build_backend")
    await tree.dispatch.admit_input(
        root.id, event_type=ET.USER, content="Take off. Ship the feature.", actor="user")

    from tests.test_task_execution import make_api_client
    with make_api_client(cfg, session_mgr, tree) as client:
        payload = delegate_payload(child.id, repo)
        first = client.post("/api/internal/delegate", json=payload, headers=OPERATOR)
        assert first.status_code == 200, first.text
        body = first.json()
        child_leaf, run_id = body["session_id"], body["run_id"]
        assert body["parent_session_id"] == child.id
        leaf_meta = await tree.load_meta(child_leaf)
        assert leaf_meta is not None and leaf_meta.profile == "worker"
        assert leaf_meta.task_parent_id == child.id
        runs = tree.runs.list_run_records_sync(child_leaf)
        assert [r.id for r in runs] == [run_id]

        # The replayed request returns the original product, never a sibling.
        replay = client.post("/api/internal/delegate", json=payload, headers=OPERATOR)
        assert replay.status_code == 200, replay.text
        assert replay.json()["session_id"] == child_leaf
        assert replay.json()["run_id"] == run_id

        # The run executes on the API loop that scheduled it: the wait stays
        # inside the client context (the same rule the execution tests pin).
        deadline = asyncio.get_event_loop().time() + 15
        while asyncio.get_event_loop().time() < deadline:
            if tree.runs.terminal_outcome(
                    tree.runs.load_events_sync(child_leaf), run_id) is not None:
                break
            await asyncio.sleep(0.05)
        else:
            pytest.fail("the delegated leaf run never reached a terminal fact")

    # ONE leaf and ONE process for the operation and its replay.
    leaves = [m.id for m in (await tree._get_index()).metas.values()
              if (await tree.load_meta(m.id)) is not None
              and (await tree.load_meta(m.id)).task_parent_id == child.id]
    assert leaves == [child_leaf]
    assert len(builds) == 1


@pytest.mark.asyncio
async def test_shadowing_local_instruction_blocks_inherited_delegation(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
    """A local real user instruction without a take-off shadows the authorized
    ancestor: the nearest node with a real user instruction is where the gate
    applies, and it fails there."""
    cfg, session_mgr, tree, root, child = await make_tree(tmp_path, monkeypatch)
    builds = install_backends(monkeypatch, [], "src.agents.worker.build_backend")
    await tree.dispatch.admit_input(
        root.id, event_type=ET.USER, content="Take off. Ship the feature.", actor="user")
    await tree.dispatch.admit_input(
        child.id, event_type=ET.USER, content="please look into this", actor="user")

    from tests.test_task_execution import make_api_client
    with make_api_client(cfg, session_mgr, tree) as client:
        resp = client.post(
            "/api/internal/delegate", json=delegate_payload(child.id, repo), headers=OPERATOR)
    assert resp.status_code == 403
    assert "no active authorization" in resp.json()["detail"]
    assert builds == []
    assert [m for m in (await tree._get_index()).metas.values()
            if m.task_parent_id == child.id] == []


@pytest.mark.asyncio
async def test_expired_pre_takeoff_on_the_ancestor_blocks(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
    cfg, session_mgr, tree, root, child = await make_tree(tmp_path, monkeypatch)
    builds = install_backends(monkeypatch, [], "src.agents.worker.build_backend")
    # The established matching: the ordinary "take off" phrase is judged on the
    # FILE-LAST real user message, so an expiring window needs a later ordinary
    # message after the stamp (the same shape the legacy gate's expiry test uses).
    issued = (datetime.now(UTC) - timedelta(hours=13)).isoformat()
    await tree.dispatch.admit_input(
        root.id, event_type=ET.USER, content="pre take off", actor="user",
        timestamp=issued)
    await tree.dispatch.admit_input(
        root.id, event_type=ET.USER, content="carry on with the plan", actor="user")

    from tests.test_task_execution import make_api_client
    with make_api_client(cfg, session_mgr, tree) as client:
        resp = client.post(
            "/api/internal/delegate", json=delegate_payload(child.id, repo), headers=OPERATOR)
    assert resp.status_code == 403
    assert "no active authorization" in resp.json()["detail"]
    assert builds == []


@pytest.mark.asyncio
async def test_agent_cron_and_report_takeoff_strings_never_authorize(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
    """No node holds a real user instruction; the child's agent/cron/report
    texts that say 'take off' mint nothing."""
    cfg, session_mgr, tree, root, child = await make_tree(tmp_path, monkeypatch)
    builds = install_backends(monkeypatch, [], "src.agents.worker.build_backend")
    await tree.dispatch.admit_input(
        child.id, event_type=ET.AGENT_MESSAGE, content="take off now", actor="agent",
        from_session=root.id, from_session_name="Project")
    await tree.dispatch.admit_input(
        child.id, event_type=ET.SCHEDULED_TRIGGER, content="take off at noon",
        actor="system", input_id="cron:job:2026-01-01T00:00:00+00:00")
    from src.core.control_events import build_control_event
    await tree.events.append(root.id, build_control_event(
        ET.CHILD_REPORT, actor="agent", source_session_id=root.id,
        child_session_id=root.id, child_event_id="00000000-0000-0000-0000-000000000000",
        outcome="completed", summary="take off"))

    from tests.test_task_execution import make_api_client
    with make_api_client(cfg, session_mgr, tree) as client:
        resp = client.post(
            "/api/internal/delegate", json=delegate_payload(child.id, repo), headers=OPERATOR)
    assert resp.status_code == 403
    assert "no real user instruction" in resp.json()["detail"]
    assert builds == []


@pytest.mark.asyncio
async def test_worker_caller_cannot_delegate(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
    """Delegating FROM a worker node is refused: agents run only under a
    manager task."""
    cfg, session_mgr, tree, root, child = await make_tree(tmp_path, monkeypatch)
    builds = install_backends(monkeypatch, [], "src.agents.worker.build_backend")
    worker = await tree.create_task(
        request_id="w", task_parent_id=child.id, profile="worker",
        task=TaskSpec(goal="leaf"), name="W", backend=None, caller="operator")
    await tree.dispatch.admit_input(
        root.id, event_type=ET.USER, content="Take off. Ship the feature.", actor="user")

    from tests.test_task_execution import make_api_client
    with make_api_client(cfg, session_mgr, tree) as client:
        resp = client.post(
            "/api/internal/delegate", json=delegate_payload(worker.id, repo), headers=OPERATOR)
    assert resp.status_code == 403
    assert "not a manager" in resp.json()["detail"]
    assert builds == []


@pytest.mark.asyncio
async def test_foreign_run_token_cannot_delegate(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
    """A run token bound to one node cannot create a worker under a different
    node: the worker-leaf constraint refuses the foreign identity."""
    cfg, session_mgr, tree, root, child = await make_tree(tmp_path, monkeypatch)
    builds = install_backends(monkeypatch, [], "src.agents.worker.build_backend")
    await tree.dispatch.admit_input(
        root.id, event_type=ET.USER, content="Take off. Ship the feature.", actor="user")
    agents_child = await tree.create_task(
        request_id="agent-leaf", task_parent_id=child.id, profile="worker",
        task=TaskSpec(goal="agent leaf"), name="AL", backend=None, caller="operator")
    await tree.runs.register_run(
        RunRecord(id="agent-run", session_id=agents_child.id, kind="work"))
    import subprocess

    from src.core.runs import read_pid_stat
    proc = subprocess.Popen(["/bin/sleep", "30"])
    pair = read_pid_stat(proc.pid)
    await tree.runs.record_launch(agents_child.id, "agent-run", pid=proc.pid, pid_start=pair[0])
    from tests.test_task_execution import make_api_client
    token = sign_run_token(
        RunTokenClaims(session_id=agents_child.id, run_id="agent-run", agent="worker"),
        "op-secret")
    with make_api_client(cfg, session_mgr, tree) as client:
        resp = client.post(
            "/api/internal/delegate", json=delegate_payload(child.id, repo),
            headers={"Authorization": f"Bearer {token}"})
    proc.terminate()
    assert resp.status_code == 403
    assert "task directly under its own" in resp.json()["detail"]
    assert builds == []


@pytest.mark.asyncio
async def test_verify_exemption_on_the_v2_route_and_launch(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
    """A read-only verify delegation needs no authorization window on the real
    v2 route and still launches (and runs) with no user instruction anywhere —
    while a repo task type under the same tree stays blocked."""
    cfg, session_mgr, tree, _root, child = await make_tree(tmp_path, monkeypatch)
    builds = install_backends(
        monkeypatch, [SpawningScriptedBackend([result_event("verdict: no")])],
        "src.agents.worker.build_backend")

    from tests.test_task_execution import make_api_client
    with make_api_client(cfg, session_mgr, tree) as client:
        verify = client.post(
            "/api/internal/delegate", json=delegate_payload(child.id, repo, task_type="verify"),
            headers=OPERATOR)
        assert verify.status_code == 200, verify.text
        verify_leaf = verify.json()["session_id"]
        verify_run = verify.json()["run_id"]

        blocked = client.post(
            "/api/internal/delegate", json=delegate_payload(child.id, repo), headers=OPERATOR)
        assert blocked.status_code == 403

        # The verify run launched and settled on the API loop that scheduled it.
        deadline = asyncio.get_event_loop().time() + 15
        while asyncio.get_event_loop().time() < deadline:
            if tree.runs.terminal_outcome(
                    tree.runs.load_events_sync(verify_leaf), verify_run) is not None:
                break
            await asyncio.sleep(0.05)
        else:
            pytest.fail("the verify run never launched without an authorization window")
        assert tree.runs.terminal_outcome(
            tree.runs.load_events_sync(verify_leaf), verify_run) == "success"
    # The verify child is repo-less; the blocked repo delegation created nothing.
    leaves = [m for m in (await tree._get_index()).metas.values()
              if m.task_parent_id == child.id]
    assert [m.id for m in leaves] == [verify_leaf]
    assert len(builds) == 1


@pytest.mark.asyncio
async def test_v1_delegate_without_takeoff_stays_blocked(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
    """A legacy session keeps its session-local gate: no take-off, no spawn,
    and the session is not rewritten into a task node."""
    cfg, session_mgr, tree, _root, _child = await make_tree(tmp_path, monkeypatch)
    builds = install_backends(monkeypatch, [], "src.agents.worker.build_backend")
    from src.core.models import CreateSessionRequest
    legacy = await session_mgr.create_session(CreateSessionRequest(name="Legacy"))

    from tests.test_task_execution import make_api_client
    with make_api_client(cfg, session_mgr, tree) as client:
        resp = client.post(
            "/api/internal/delegate", json=delegate_payload(legacy.id, repo), headers=OPERATOR)
    assert resp.status_code == 403
    assert "no active authorization" in resp.json()["detail"]
    assert builds == []
    stays_legacy = await session_mgr.get_session(legacy.id)
    assert stays_legacy is not None and stays_legacy.profile is None
    assert stays_legacy.schema_version == 1


@pytest.mark.asyncio
async def test_legacy_root_authorizes_its_worker_child_from_its_own_takeoff(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
    """The gate treats a legacy root as a manager node whose chat log is the
    authorization source: a worker child created under it launches on the
    root's real user take-off, and the session itself is never rewritten."""
    _cfg, session_mgr, tree, _root, _child = await make_tree(tmp_path, monkeypatch)
    from src.core.models import CreateSessionRequest
    legacy = await session_mgr.create_session(CreateSessionRequest(name="Legacy"))
    await session_mgr.save_chat_event(legacy.id, _legacy_user_takeoff())
    child = await tree.create_task(
        request_id="legacy-child", task_parent_id=legacy.id, profile="worker",
        task=TaskSpec(goal="do the thing", repo_path=str(repo)),
        name=None, backend=None, caller=OPERATOR)
    assert legacy.profile is None and legacy.schema_version == 1

    # The start node is the worker's parent — the launch gate's own shape.
    assert await tree.check_task_authorization(child.task_parent_id) == legacy.id

    # Without any real user instruction on the root the same walk blocks.
    bare = await session_mgr.create_session(CreateSessionRequest(name="Bare legacy"))
    bare_child = await tree.create_task(
        request_id="bare-child", task_parent_id=bare.id, profile="worker",
        task=TaskSpec(goal="do the other thing", repo_path=str(repo)),
        name=None, backend=None, caller=OPERATOR)
    from src.core.takeoff_gate import DelegationBlockedError
    with pytest.raises(DelegationBlockedError, match="no real user instruction"):
        await tree.check_task_authorization(bare_child.task_parent_id)

    # A user message without a take-off blocks at the root too.
    plain = await session_mgr.create_session(CreateSessionRequest(name="Plain legacy"))
    await session_mgr.save_chat_event(plain.id, _legacy_user_takeoff("Just a question, no go."))
    plain_child = await tree.create_task(
        request_id="plain-child", task_parent_id=plain.id, profile="worker",
        task=TaskSpec(goal="and another", repo_path=str(repo)),
        name=None, backend=None, caller=OPERATOR)
    with pytest.raises(DelegationBlockedError, match="no active authorization"):
        await tree.check_task_authorization(plain_child.task_parent_id)


def _legacy_user_takeoff(content: str = "Take off. Ship it.") -> dict:
    return {"id": "user-takeoff", "type": ET.USER, "content": content, "actor": "user",
            "timestamp": datetime.now(UTC).isoformat()}


@pytest.mark.asyncio
async def test_legacy_session_delegates_in_place(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
    """A legacy session whose own chat carries a take-off delegates through the
    real route without being rewritten: its metadata.json is byte-identical
    after the create, the worker leaf hangs under it with one launched Run,
    and the reply carries the task-tree shape."""
    cfg, session_mgr, tree, _root, _child = await make_tree(tmp_path, monkeypatch)
    builds = install_backends(
        monkeypatch, [SpawningScriptedBackend([result_event("leaf done")])],
        "src.agents.worker.build_backend")
    from src.core.models import CreateSessionRequest
    legacy = await session_mgr.create_session(CreateSessionRequest(name="Legacy"))
    await session_mgr.save_chat_event(legacy.id, _legacy_user_takeoff())
    meta_path = cfg.sessions_dir / legacy.id / "metadata.json"
    before = meta_path.read_bytes()

    from tests.test_task_execution import make_api_client
    with make_api_client(cfg, session_mgr, tree) as client:
        first = client.post(
            "/api/internal/delegate", json=delegate_payload(legacy.id, repo), headers=OPERATOR)
        assert first.status_code == 200, first.text
        body = first.json()
        assert body["parent_session_id"] == legacy.id
        leaf_id, run_id = body["session_id"], body["run_id"]
        assert body["thread_id"] == run_id

        assert meta_path.read_bytes() == before
        stays_legacy = await session_mgr.get_session(legacy.id)
        assert stays_legacy is not None
        assert stays_legacy.profile is None and stays_legacy.schema_version == 1

        leaf = await tree.load_meta(leaf_id)
        assert leaf is not None and leaf.profile == "worker" and leaf.task_parent_id == legacy.id
        assert leaf.task is not None and leaf.task.task_type == "quick-edit"
        assert [r.id for r in tree.runs.list_run_records_sync(leaf_id)] == [run_id]

        deadline = asyncio.get_event_loop().time() + 15
        while asyncio.get_event_loop().time() < deadline:
            if tree.runs.terminal_outcome(tree.runs.load_events_sync(leaf_id), run_id) is not None:
                break
            await asyncio.sleep(0.05)
        else:
            pytest.fail("the delegated leaf run never reached a terminal fact")
    assert len(builds) == 1


@pytest.mark.asyncio
async def test_legacy_verify_delegation_records_the_task_type_without_adopting(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
    """A read-only verify delegation from a legacy session needs no take-off,
    never rewrites the session, and records the verify task type on the
    repo-less leaf."""
    cfg, session_mgr, tree, _root, _child = await make_tree(tmp_path, monkeypatch)
    builds = install_backends(
        monkeypatch, [SpawningScriptedBackend([result_event("verdict: yes")])],
        "src.agents.worker.build_backend")
    from src.core.models import CreateSessionRequest
    legacy = await session_mgr.create_session(CreateSessionRequest(name="Legacy"))

    from tests.test_task_execution import make_api_client
    with make_api_client(cfg, session_mgr, tree) as client:
        verify = client.post(
            "/api/internal/delegate", json=delegate_payload(legacy.id, repo, task_type="verify"),
            headers=OPERATOR)
        assert verify.status_code == 200, verify.text
        leaf_id, run_id = verify.json()["session_id"], verify.json()["run_id"]
        stays_legacy = await session_mgr.get_session(legacy.id)
        assert stays_legacy is not None
        assert stays_legacy.profile is None and stays_legacy.schema_version == 1
        leaf = await tree.load_meta(leaf_id)
        assert leaf is not None and leaf.task is not None
        assert leaf.task.task_type == "verify" and leaf.task.repo_path is None

        deadline = asyncio.get_event_loop().time() + 15
        while asyncio.get_event_loop().time() < deadline:
            if tree.runs.terminal_outcome(tree.runs.load_events_sync(leaf_id), run_id) is not None:
                break
            await asyncio.sleep(0.05)
        else:
            pytest.fail("the verify run never launched")
    assert len(builds) == 1
