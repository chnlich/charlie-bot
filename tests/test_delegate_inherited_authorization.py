"""Inherited authorization on the real v2 entry points.

The accepted v2 policy is one central owner: the nearest-real-user-ancestor
gate (``TaskTreeManager.check_task_authorization``), applied at the request
entries — delegation, improve, and agent messages to worker nodes — and never
re-judged at a Run's launch. A sub-task manager under an authorized project
delegates through the real /api/internal/delegate route without carrying its
own take-off; a local user instruction shadows the ancestor; agent, cron, and
report texts never mint authorization; verify stays exempt (read-only) on the
route; v1 semantics are untouched. Every scenario here runs the actual HTTP
route against a synthetic instance with a scripted backend.
"""

from __future__ import annotations

from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest
from conftest import WORKER_BUILD_BACKEND_PATCH_TARGET, agent_headers, delegate_payload, stub_credentials

from src.core import event_types as ET
from src.core.models import RunRecord, TaskSpec
from src.core.run_token import RunTokenClaims, sign_run_token
from tests.test_task_execution import (
    OPERATOR,
    SpawningScriptedBackend,
    _adapter_with_silent_broadcast,
    build_env,
    install_backends,
    result_event,
    wait_for_terminal_run,
)


async def make_tree(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    """One synthetic instance: root manager + child manager under it, plus the
    API client and the scripted executor, with no user input anywhere yet."""
    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
    from conftest import patch_instructions_content
    patch_instructions_content(monkeypatch)
    stub_credentials({"charliebot": {"access_key": "op-secret"}})
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


@pytest.mark.asyncio
@pytest.mark.integration  # polls the delegated run's terminal fact in real time on the API loop
@pytest.mark.asyncio
async def test_authorized_ancestor_delegates_without_a_local_takeoff(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
    """The real route from a child manager with NO local user message and an
    authorized ancestor creates one leaf and one launched Run; a replay returns
    the same child/Run and never spawns a second process."""
    cfg, session_mgr, tree, root, child = await make_tree(tmp_path, monkeypatch)
    builds = install_backends(
        monkeypatch, [SpawningScriptedBackend([result_event("leaf done")])],
        WORKER_BUILD_BACKEND_PATCH_TARGET)
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
        await wait_for_terminal_run(tree, child_leaf, run_id)

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
    builds = install_backends(monkeypatch, [], WORKER_BUILD_BACKEND_PATCH_TARGET)
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
    builds = install_backends(monkeypatch, [], WORKER_BUILD_BACKEND_PATCH_TARGET)
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
async def test_worker_caller_cannot_delegate(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
    """Delegating FROM a worker node is refused: agents run only under a
    manager task."""
    cfg, session_mgr, tree, root, child = await make_tree(tmp_path, monkeypatch)
    builds = install_backends(monkeypatch, [], WORKER_BUILD_BACKEND_PATCH_TARGET)
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
    builds = install_backends(monkeypatch, [], WORKER_BUILD_BACKEND_PATCH_TARGET)
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
        WORKER_BUILD_BACKEND_PATCH_TARGET)

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
        _run, outcome = await wait_for_terminal_run(tree, verify_leaf, verify_run)
        assert outcome == "success"
    # The verify child is repo-less; the blocked repo delegation created nothing.
    leaves = [m for m in (await tree._get_index()).metas.values()
              if m.task_parent_id == child.id]
    assert [m.id for m in leaves] == [verify_leaf]
    assert len(builds) == 1


async def register_active_run(tree, session_id: str, run_id: str, kind: str = "manager_turn") -> None:
    """Pin one active, launched Run on *session_id*: the identity its run
    token stands for (registered, no terminal fact, launch identity pinned)."""
    await tree.runs.register_run(RunRecord(id=run_id, session_id=session_id, kind=kind))
    await tree.runs.record_launch(session_id, run_id, pid=424242, pid_start=f"ps-{run_id}")


@pytest.mark.asyncio
async def test_agent_run_token_delegates_verify_without_a_takeoff(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
    """The CLI's real credential — a run token — delegating a read-only verify
    needs no takeoff window: with no user instruction anywhere, the delegation
    returns 200, creates a repo-less verify child, and its run launches and
    settles (the agent-creation check reads the same verify exemption the
    route and the launch read)."""
    cfg, session_mgr, tree, _root, child = await make_tree(tmp_path, monkeypatch)
    builds = install_backends(
        monkeypatch, [SpawningScriptedBackend([result_event("verdict: yes")])],
        WORKER_BUILD_BACKEND_PATCH_TARGET)
    await register_active_run(tree, child.id, "child-run")

    from tests.test_task_execution import make_api_client
    with make_api_client(cfg, session_mgr, tree) as client:
        verify = client.post(
            "/api/internal/delegate", json=delegate_payload(child.id, repo, task_type="verify"),
            headers=agent_headers(child.id, "child-run"))
        assert verify.status_code == 200, verify.text
        leaf_id, run_id = verify.json()["session_id"], verify.json()["run_id"]
        leaf = await tree.load_meta(leaf_id)
        assert leaf is not None and leaf.profile == "worker"
        assert leaf.task_parent_id == child.id
        assert leaf.task is not None
        assert leaf.task.task_type == "verify" and leaf.task.repo_path is None
        _run, outcome = await wait_for_terminal_run(tree, leaf_id, run_id)
    assert outcome == "success"
    assert len(builds) == 1
