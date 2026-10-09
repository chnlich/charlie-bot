"""A reviewed work Run lands on the branch its reviewer publishes: origin/<base>.

The reviewer's push step (``git push origin HEAD:<base>``) updates the published
branch only, so a shared clone's local <base> can lag it. The post-review
landing judgment reads the fetched origin/<base> tip, and a failed judgment
carries git's reason into the parent's blocked report. A delivered child_report
reaches the manager's turn text with its summary after the typed header.
"""

from __future__ import annotations

import asyncio
import functools
import pathlib
import subprocess

import conftest
import pytest

from src.infra import event_types as ET
from src.infra import models
from src.runtime import task_sessions
from tests import test_task_execution


async def _reviewed_delivery(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, *,
                             reviewer_pushes: bool) -> tuple[task_sessions.TaskTreeManager, str, str, pathlib.Path]:
  """One implement task based on the bare ``main``, through work and a successful review.

  The work backend commits a marker on the work branch. When *reviewer_pushes*,
  the review backend publishes the branch the way the reviewer's push step
  does, which moves origin's main and leaves the clone's local main behind.
  Returns once the review Run's delivery chain has reported to the manager.
  """
  cfg, session_blocks, tree = test_task_execution.build_env(tmp_path, monkeypatch)
  repo, _origin = conftest.init_repo_with_origin(tmp_path)
  tree.dispatch.executor = test_task_execution._adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  manager = await conftest.create_task(tree, parent=None, request_id="root")
  worker = await conftest.create_task(
      tree,
      parent=manager.id,
      request_id="w",
      profile="worker",
      task=models.TaskSpec(
          goal="add a marker file", repo_path=str(repo), base_branch="main", task_type=models.TaskType.IMPLEMENT))

  def manager_turn(option, cfg_, **kwargs):
    backend = test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("manager turn")])
    if kwargs.get("on_spawn") is not None:
      backend.set_on_spawn(kwargs["on_spawn"])
    return backend

  monkeypatch.setattr(conftest.BUILD_BACKEND_PATCH_TARGET, manager_turn)
  # The work-run launch judges the nearest-user authorization on the manager.
  await tree.dispatch.admit_input(manager.id, event_type=ET.USER, content="Take off and add the marker.", actor="user")

  def publish() -> None:
    if reviewer_pushes:
      conftest.run_git(test_task_execution.work_run_worktree(tree, worker.id), "push", "-q", "origin", "HEAD:main")

  conftest.install_scripted_backends(
      monkeypatch, [
          test_task_execution.SpawningScriptedBackend(
              [test_task_execution.result_event("implemented")],
              pre_run=functools.partial(test_task_execution.implement_marker_commit, tree, worker.id)),
          test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("review ok")], pre_run=publish),
      ], conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)
  await tree.dispatch.admit_input(worker.id, event_type=ET.USER, content="Start the work.", actor="user")
  decision = await tree.dispatch.dispatch_pending(worker.id)
  _work, outcome = await test_task_execution.wait_for_terminal_run(tree, worker.id, decision["run_id"])
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


def _reports(tree: task_sessions.TaskTreeManager, manager_id: str) -> list[dict]:
  return [e for e in tree.events.load_events(manager_id) if e.get("type") == ET.CHILD_REPORT]


@pytest.mark.asyncio
async def test_commit_published_only_on_origin_base_closes_the_reviewed_task(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  tree, manager_id, worker_id, repo = await _reviewed_delivery(tmp_path, monkeypatch, reviewer_pushes=True)

  work = next(r for r in tree.runs.list_run_records_sync(worker_id) if r.kind == "work")
  assert work.base_branch == "main" and work.branch_name
  commit = conftest.run_git(repo, "rev-parse", work.branch_name)
  # The premise: the reviewed commit is on the published main only; the
  # clone's local main still sits at the seed commit.
  assert conftest.run_git(repo, "rev-parse", "origin/main") == commit
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
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  tree, manager_id, worker_id, _repo = await _reviewed_delivery(tmp_path, monkeypatch, reviewer_pushes=False)

  reports = _reports(tree, manager_id)
  assert [r["outcome"] for r in reports] == ["blocked"]
  summary = reports[0]["summary"]
  assert "passed review but its branch did not land on main: " in summary
  assert "ancestry check failed" in summary and "(origin/main)" in summary
  assert tree.task_state(worker_id) == "open"


@pytest.mark.asyncio
async def test_blocked_child_report_reaches_the_manager_turn_with_its_summary(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_blocks, tree = test_task_execution.build_env(tmp_path, monkeypatch)
  tree.dispatch.executor = test_task_execution._adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  manager = await conftest.create_task(tree, parent=None, request_id="root")
  child = await conftest.create_task(
      tree, parent=manager.id, request_id="child", profile="worker", task=models.TaskSpec(goal="work"))
  builds = conftest.install_scripted_backends(
      monkeypatch, [test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("noted")])],
      conftest.BUILD_BACKEND_PATCH_TARGET)
  summary = "work run run-w passed review but its branch did not land on main: ancestry check failed"
  await tree.dispatch.deliver_child_report(
      child.id, source_event={"id": "finish-1"}, outcome="blocked", summary=summary, recipient=manager.id)

  decision = await tree.dispatch.dispatch_pending(manager.id)
  run, _outcome = await test_task_execution.wait_for_terminal_run(tree, manager.id, decision["run_id"])

  line = f"[Report from task {child.id} | outcome blocked] {summary}"
  launch_text = (tree.runs.run_dir(manager.id, run.id) / "launch_prompt.md").read_text(encoding="utf-8")
  assert line in launch_text
  assert line in builds[0]["backend"].prompt
