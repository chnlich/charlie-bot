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
from conftest import patch_instructions_content

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
    cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
    tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
    repo, _origin = init_repo_with_origin(tmp_path / "repo")
    manager = await tree.create_task(
        request_id="root", task_parent_id=None, profile="manager",
        task=TaskSpec(goal="pm"), name="PM", backend=None, caller="operator")
    install_backends(monkeypatch, [SpawningScriptedBackend([result_event("taken off")])],
                     "src.agents.backends.registry.build_backend")
    patch_instructions_content(monkeypatch)
    await tree.dispatch.admit_input(
        manager.id, event_type=ET.USER, content="Take off.", actor="user")
    decision = await tree.dispatch.dispatch_pending(manager.id)
    await wait_for_terminal_run(tree, manager.id, decision["run_id"])

    child = await tree.create_task(
        request_id="child", task_parent_id=manager.id, profile="worker",
        task=TaskSpec(goal="do the work", repo_path=str(repo), task_type=TaskType.IMPLEMENT),
        name="W", backend=None, caller="operator")
    # The review chain: the work run succeeds, its reviewer fails on every
    # backend (blocked report to the parent), and the child stays open.
    install_backends(monkeypatch, [
        SpawningScriptedBackend([result_event("work done")]),
        SpawningScriptedBackend([result_event("review approved")]),
    ], "src.agents.worker.build_backend")
    await tree.dispatch.admit_input(
        child.id, event_type=ET.USER, content="Start the work.", actor="user")
    d1 = await tree.dispatch.dispatch_pending(child.id)
    work_run_id = d1["run_id"]
    await wait_for_terminal_run(tree, child.id, work_run_id)

    # An input admitted while the review Run held the serialized slot; the
    # landing then cannot complete (the origin vanished), so the review chain
    # ends with a blocked report, the child stays open, and no failed run
    # blocks fresh dispatch.
    await tree.dispatch.admit_input(
        child.id, event_type=ET.AGENT_MESSAGE, content="one more tweak", actor="agent")
    review_runs = [r for r in tree.runs.list_run_records_sync(child.id) if r.kind == "review"]
    assert len(review_runs) == 1
    review_run = review_runs[0]
    # The landing then cannot complete: the origin vanishes while the review
    # still runs (reviews reuse the work run's worktree and need no origin), so
    # the review chain deterministically ends with a blocked report.
    ok, _err = await git._run_git_cmd(repo, "remote", "remove", "origin",
                                      timeout=10, timeout_label="remove-origin")
    assert ok
    deadline = asyncio.get_event_loop().time() + 10
    while asyncio.get_event_loop().time() < deadline:
        if tree.runs.terminal_outcome(
                tree.runs.load_events_sync(child.id), review_run.id) is not None:
            break
        await asyncio.sleep(0.05)

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
    # ...and the stranded input gets the next permitted serialized dispatch: a
    # fresh work Run claims it (the review path's early return used to leave it
    # pending forever).
    await tree.dispatch.dispatch_pending(child.id)
    records = tree.runs.list_run_records_sync(child.id)
    next_runs = [r for r in records if r.kind == "work" and r.id != work_run_id]
    assert next_runs, f"the input admitted during the review was stranded: {records}"
    assert next_runs[0].input_event_ids, "the next dispatch did not claim the admitted input"
