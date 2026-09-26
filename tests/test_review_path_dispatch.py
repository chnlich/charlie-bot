"""Inputs admitted during a review Run must reach the node's own dispatcher.

The review path's early return used to strand inputs admitted while a review
Run was consuming the node's serialized slot: the parent got the failure
report, the child's own pending input never saw a dispatch decision, and the
next permitted serialized dispatch never came.
"""

from __future__ import annotations

import asyncio
from pathlib import Path

import pytest
from conftest import BUILD_BACKEND_PATCH_TARGET, patch_instructions_content

from src.core import event_types as ET
from src.core import git
from src.core.models import TaskSpec, TaskType
from tests.test_task_execution import (
    SpawningScriptedBackend,
    _adapter_with_silent_broadcast,
    build_env,
    init_repo_with_origin,
    install_backends,
    result_event,
    wait_for_terminal_run,
)


@pytest.mark.asyncio
async def test_input_admitted_during_a_failed_review_gets_the_next_dispatch(
        tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """An input admitted while the review Run holds the serialized slot gets
    the next permitted serialized dispatch after the review chain finishes.

    Deterministic by construction: the review backend is GATED, so the admit
    and the landing-defeating commit provably land during the review — no race
    against the scripted review's finish. The review itself succeeds; what
    blocks delivery is the work branch's commit not landing on main (an empty
    commit on the branch, made while the review runs), so the chain reports
    "passed review but its branch did not land" and the child stays open.
    """
    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
    tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
    repo, _origin = init_repo_with_origin(tmp_path / "repo")
    manager = await tree.create_task(
        request_id="root", task_parent_id=None, profile="manager",
        task=TaskSpec(goal="pm"), name="PM", backend=None, caller="operator")
    install_backends(monkeypatch, [SpawningScriptedBackend([result_event("taken off")])],
                     BUILD_BACKEND_PATCH_TARGET)
    patch_instructions_content(monkeypatch)
    await tree.dispatch.admit_input(
        manager.id, event_type=ET.USER, content="Take off.", actor="user")
    decision = await tree.dispatch.dispatch_pending(manager.id)
    await wait_for_terminal_run(tree, manager.id, decision["run_id"])

    child = await tree.create_task(
        request_id="child", task_parent_id=manager.id, profile="worker",
        task=TaskSpec(goal="do the work", repo_path=str(repo), task_type=TaskType.IMPLEMENT),
        name="W", backend=None, caller="operator")
    review_gate = asyncio.Event()
    install_backends(monkeypatch, [
        SpawningScriptedBackend([result_event("work done")]),
        # The gated review: it cannot finish before the test's mid-review
        # actions below, so they are provably "during the review Run".
        SpawningScriptedBackend([result_event("review approved")], gate=review_gate.wait),
        SpawningScriptedBackend([result_event("tweak done")]),
        # The tweak's own delivery chain spawns its review of the tweak run.
        SpawningScriptedBackend([result_event("tweak review approved")]),
    ], "src.agents.worker.build_backend")
    await tree.dispatch.admit_input(
        child.id, event_type=ET.USER, content="Start the work.", actor="user")
    d1 = await tree.dispatch.dispatch_pending(child.id)
    work_run_id = d1["run_id"]
    await wait_for_terminal_run(tree, child.id, work_run_id)

    review_runs = [r for r in tree.runs.list_run_records_sync(child.id) if r.kind == "review"]
    assert len(review_runs) == 1
    review_run = review_runs[0]
    # The registered gated review holds the node's serialized slot and cannot
    # finish before the gate below releases — everything here is provably
    # "during the review Run" without a spawn-timing wait.
    assert tree.runs.terminal_outcome(
        tree.runs.load_events_sync(child.id), review_run.id) is None

    # DURING the review: admit the input and leave a commit on the work branch
    # that never lands on main, so the successful review cannot complete
    # delivery (its landing check honestly fails).
    await tree.dispatch.admit_input(
        child.id, event_type=ET.AGENT_MESSAGE, content="one more tweak", actor="agent")
    work_run = await tree.runs.get_run(child.id, work_run_id)
    assert work_run is not None and work_run.worktree_path
    ok, _err = await git._run_git_cmd(
        Path(work_run.worktree_path), "commit", "--allow-empty", "-q", "-m", "unlanded wip",
        timeout=10, timeout_label="unlanded-wip")
    assert ok
    review_gate.set()

    deadline = asyncio.get_event_loop().time() + 10
    while asyncio.get_event_loop().time() < deadline:
        if tree.runs.terminal_outcome(
                tree.runs.load_events_sync(child.id), review_run.id) is not None:
            break
        await asyncio.sleep(0.05)
    else:
        pytest.fail("the review Run never reached its terminal fact")

    # The blocked report reached the parent; the child is still open...
    reports: list[dict] = []
    deadline = asyncio.get_event_loop().time() + 15
    while asyncio.get_event_loop().time() < deadline:
        reports = [e for e in tree.events.load_events(manager.id)
                   if e.get("type") == ET.CHILD_REPORT]
        if any("review" in str(e.get("summary", "")).lower() for e in reports):
            break
        await asyncio.sleep(0.1)
    assert any("review" in str(e.get("summary", "")).lower() for e in reports), reports
    assert tree.task_state(child.id) == "open"
    # ...and the stranded input got the next permitted serialized dispatch: a
    # fresh work Run claimed and consumed it (the review path's early return
    # used to leave it pending forever).
    deadline = asyncio.get_event_loop().time() + 15
    while asyncio.get_event_loop().time() < deadline:
        next_runs = [r for r in tree.runs.list_run_records_sync(child.id)
                     if r.kind == "work" and r.id != work_run_id]
        if next_runs and tree.runs.terminal_outcome(
                tree.runs.load_events_sync(child.id), next_runs[0].id) is not None:
            break
        await asyncio.sleep(0.1)
    records = tree.runs.list_run_records_sync(child.id)
    next_runs = [r for r in records if r.kind == "work" and r.id != work_run_id]
    assert next_runs, f"the input admitted during the review was stranded: {records}"
    assert next_runs[0].input_event_ids, "the next dispatch did not claim the admitted input"
    assert tree.runs.terminal_outcome(
        tree.runs.load_events_sync(child.id), next_runs[0].id) == "success"
