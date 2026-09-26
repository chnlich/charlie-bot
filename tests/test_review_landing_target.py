"""A reviewed work Run lands on the branch its reviewer publishes: origin/<base>.

The reviewer's push step (``git push origin HEAD:<base>``) updates the published
branch only, so a shared clone's local <base> can lag it. The post-review
landing judgment reads the fetched origin/<base> tip, and a failed judgment
carries git's reason into the parent's blocked report. A delivered child_report
reaches the manager's turn text with its summary after the typed header.
"""

from __future__ import annotations

import asyncio
import subprocess
from pathlib import Path

import pytest
from conftest import (
    BUILD_BACKEND_PATCH_TARGET,
    WORKER_BUILD_BACKEND_PATCH_TARGET,
    create_task,
    patch_instructions_content,
)

from src.core import event_types as ET
from src.core.models import TaskSpec, TaskType
from src.core.task_sessions import TaskTreeManager
from tests.test_task_execution import (
    SpawningScriptedBackend,
    _adapter_with_silent_broadcast,
    build_env,
    git,
    init_repo_with_origin,
    install_backends,
    result_event,
    wait_for_terminal_run,
)


async def _reviewed_delivery(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, *,
                             reviewer_pushes: bool) -> tuple[TaskTreeManager, str, str, Path]:
  """One implement task based on the bare ``main``, through work and a successful review.

  The work backend commits a marker on the work branch. When *reviewer_pushes*,
  the review backend publishes the branch the way the reviewer's push step
  does, which moves origin's main and leaves the clone's local main behind.
  Returns once the review Run's delivery chain has reported to the manager.
  """
  cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
  repo, _origin = init_repo_with_origin(tmp_path)
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
  manager = await create_task(tree, parent=None, request_id="root")
  worker = await create_task(
      tree,
      parent=manager.id,
      request_id="w",
      profile="worker",
      task=TaskSpec(goal="add a marker file", repo_path=str(repo), base_branch="main", task_type=TaskType.IMPLEMENT))

  def manager_turn(option, cfg_, **kwargs):
    backend = SpawningScriptedBackend([result_event("manager turn")])
    if kwargs.get("on_spawn") is not None:
      backend.set_on_spawn(kwargs["on_spawn"])
    return backend

  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, manager_turn)
  patch_instructions_content(monkeypatch)
  # The work-run launch judges the nearest-user authorization on the manager.
  await tree.dispatch.admit_input(manager.id, event_type=ET.USER, content="Take off and add the marker.", actor="user")

  def worktree() -> Path:
    work = [r for r in tree.runs.list_run_records_sync(worker.id) if r.kind == "work"]
    assert len(work) == 1 and work[0].worktree_path
    return Path(work[0].worktree_path)

  def implement() -> None:
    wt = worktree()
    (wt / "marker.txt").write_text("implemented\n")
    git(wt, "add", "-A")
    git(wt, "-c", "user.email=t@example.com", "-c", "user.name=t", "commit", "-q", "-m", "implement marker")

  def publish() -> None:
    if reviewer_pushes:
      git(worktree(), "push", "-q", "origin", "HEAD:main")

  install_backends(
      monkeypatch, [
          SpawningScriptedBackend([result_event("implemented")], pre_run=implement),
          SpawningScriptedBackend([result_event("review ok")], pre_run=publish),
      ], WORKER_BUILD_BACKEND_PATCH_TARGET)
  await tree.dispatch.admit_input(worker.id, event_type=ET.USER, content="Start the work.", actor="user")
  decision = await tree.dispatch.dispatch_pending(worker.id)
  _work, outcome = await wait_for_terminal_run(tree, worker.id, decision["run_id"])
  assert outcome == "success"

  deadline = asyncio.get_event_loop().time() + 20
  while asyncio.get_event_loop().time() < deadline:
    if [e for e in tree.events.load_events(manager.id) if e.get("type") == ET.CHILD_REPORT]:
      break
    await asyncio.sleep(0.05)
  else:
    pytest.fail("the review chain never reported to the manager")
  reviews = [r for r in tree.runs.list_run_records_sync(worker.id) if r.kind == "review"]
  assert len(reviews) == 1
  assert tree.runs.terminal_outcome(tree.runs.load_events_sync(worker.id), reviews[0].id) == "success"
  return tree, manager.id, worker.id, repo


def _reports(tree: TaskTreeManager, manager_id: str) -> list[dict]:
  return [e for e in tree.events.load_events(manager_id) if e.get("type") == ET.CHILD_REPORT]


@pytest.mark.asyncio
async def test_commit_published_only_on_origin_base_closes_the_reviewed_task(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  tree, manager_id, worker_id, repo = await _reviewed_delivery(tmp_path, monkeypatch, reviewer_pushes=True)

  work = next(r for r in tree.runs.list_run_records_sync(worker_id) if r.kind == "work")
  assert work.base_branch == "main" and work.branch_name
  commit = git(repo, "rev-parse", work.branch_name)
  # The premise: the reviewed commit is on the published main only; the
  # clone's local main still sits at the seed commit.
  assert git(repo, "rev-parse", "origin/main") == commit
  assert subprocess.run(["git", "-C", str(repo), "merge-base", "--is-ancestor", commit, "main"]).returncode == 1

  deadline = asyncio.get_event_loop().time() + 10
  while asyncio.get_event_loop().time() < deadline and tree.task_state(worker_id) != "completed":
    await asyncio.sleep(0.05)
  reports = _reports(tree, manager_id)
  assert tree.task_state(worker_id) == "completed", reports
  assert [r["outcome"] for r in reports] == ["completed"]
  # The recorded landing names the published target, so the close's
  # re-verification judged origin/main too.
  close = tree.facts_of(worker_id).close_events[-1]
  assert f"landed:origin/main@{commit}" in close["result_refs"]


@pytest.mark.asyncio
async def test_commit_on_neither_base_ref_reports_blocked_with_the_git_reason(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  tree, manager_id, worker_id, _repo = await _reviewed_delivery(tmp_path, monkeypatch, reviewer_pushes=False)

  reports = _reports(tree, manager_id)
  assert [r["outcome"] for r in reports] == ["blocked"]
  summary = reports[0]["summary"]
  assert "passed review but its branch did not land on main: " in summary
  assert "ancestry check failed" in summary and "(origin/main)" in summary
  assert tree.task_state(worker_id) == "open"


@pytest.mark.asyncio
async def test_blocked_child_report_reaches_the_manager_turn_with_its_summary(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
  manager = await create_task(tree, parent=None, request_id="root")
  child = await create_task(tree, parent=manager.id, request_id="child", profile="worker", task=TaskSpec(goal="work"))
  builds = install_backends(monkeypatch, [SpawningScriptedBackend([result_event("noted")])], BUILD_BACKEND_PATCH_TARGET)
  patch_instructions_content(monkeypatch)
  summary = "work run run-w passed review but its branch did not land on main: ancestry check failed"
  await tree.dispatch.deliver_child_report(
      child.id, source_event={"id": "finish-1"}, outcome="blocked", summary=summary, recipient=manager.id)

  decision = await tree.dispatch.dispatch_pending(manager.id)
  run, _outcome = await wait_for_terminal_run(tree, manager.id, decision["run_id"])

  line = f"[Report from task {child.id} | outcome blocked] {summary}"
  launch_text = (tree.runs.run_dir(manager.id, run.id) / "launch_prompt.md").read_text(encoding="utf-8")
  assert line in launch_text
  assert line in builds[0]["backend"].prompt
