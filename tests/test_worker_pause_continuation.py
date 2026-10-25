"""Worker/reviewer pause and continuation: a run that ends while its node waits is a pause, not a delivery.

A run ending with a pending trigger (its triggers directory) or a pending
input (facts no Run claimed) does not deliver: no review spawn, no merge, no
auto-complete, no worktree cleanup, and the task stays open. The next run
continues the node's conversation, branch and worktree — a review
continuation when the latest Run is a successful review whose merge never
started, a work continuation otherwise. Ending with nothing pending delivers
by the standing rules.
"""

from __future__ import annotations

import asyncio
import datetime
import json
import pathlib
from unittest import mock

import conftest
import pytest

from src.infra import event_types as ET
from src.infra import models
from src.runtime import task_execution, triggers
from tests import test_task_execution


class NativeIdBackend(test_task_execution.SpawningScriptedBackend):
  """A scripted backend that leaves the raw transport log's session-attach line behind.

  Real backends tee their raw stdout to ``agent.raw.ndjson``, and the finalize
  path scans that file for the run's native session id; the plain double
  writes no raw file, so a run that must carry a native id (every paused run
  whose continuation should resume its conversation) writes the one raw line
  the scan reads.

  The line is Claude's ``system/init`` event. The finalize scan reads it
  without a translation, so the test needs no agent CLI binary. A Codex
  ``thread.started`` line would need the Codex backend built to translate it.
  """

  def __init__(self, *args, native_id: str, **kwargs) -> None:
    super().__init__(*args, **kwargs)
    self._native_id = native_id
    self.raw_log_dir: str | None = None

  async def run(self, prompt, cwd, env, uploaded_files=None):
    async for event in super().run(prompt, cwd, env, uploaded_files=uploaded_files):
      yield event
    if self.raw_log_dir is not None:
      line = json.dumps({"type": "system", "subtype": "init", "session_id": self._native_id}) + "\n"
      (pathlib.Path(self.raw_log_dir) / "agent.raw.ndjson").write_text(line, encoding="utf-8")


def install_backends(monkeypatch: pytest.MonkeyPatch, backends: list) -> list[dict]:
  """conftest.install_scripted_backends plus the log_dir hand-off NativeIdBackend needs."""
  builds: list[dict] = []
  queue = list(backends)

  def fake_build(option, cfg_, **kwargs):
    backend = queue.pop(0)
    if kwargs.get("on_spawn") is not None:
      backend.set_on_spawn(kwargs["on_spawn"])
    if isinstance(backend, NativeIdBackend):
      backend.raw_log_dir = kwargs.get("log_dir")
    builds.append({"option": option, "kwargs": kwargs, "backend": backend})
    return backend

  monkeypatch.setattr(conftest.WORKER_BUILD_BACKEND_PATCH_TARGET, fake_build)
  return builds


def _trigger(session_id: str, trigger_id: str = "trig-1", message: str = "build finished") -> models.PendingTrigger:
  return models.PendingTrigger(
      id=trigger_id, session_id=session_id, fire_at=datetime.datetime.now(datetime.UTC), message=message)


def _patch_parent_turn_build(monkeypatch: pytest.MonkeyPatch) -> None:
  """Patch the shared build_backend for parent manager turns without starving translate builds.

  The patch target (``backend_types.build_backend``) is also what the run
  finalize path builds its translate instance through; a build without an
  ``on_spawn`` is translate-only and gets the real factory, or the finalize's
  translate falls back to passthrough and no native session id is ever
  recorded. Launcher builds (on_spawn set) get the scripted parent double.
  """
  from src.runtime.hooks import backend_types
  real_build = backend_types.build_backend

  def build(option, cfg_, **kwargs):
    if kwargs.get("on_spawn") is None:
      return real_build(option, cfg_, **kwargs)
    backend = test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("manager turn")])
    backend.set_on_spawn(kwargs["on_spawn"])
    return backend

  monkeypatch.setattr(conftest.BUILD_BACKEND_PATCH_TARGET, build)


async def _worker_child(
    tmp_path: pathlib.Path,
    monkeypatch: pytest.MonkeyPatch,
    *,
    task_type: str = "implement",
    with_repo: bool = True,
    backend_options: list | None = None,
):
  """(cfg, tree, manager, worker, repo): a worker child under a manager root with its
    takeoff authorization, the silent-broadcast executor and the parent-turn build double.

    ``backend_options`` configures several backends; the pool follows their
    order, so the reviewer is the first option that is not the worker's."""
  if backend_options is None:
    # Two members: the reviewer never uses the worker's member, so the review takes the second.
    cfg, session_blocks, tree = test_task_execution.build_env(
        tmp_path, monkeypatch, backend_ids=["fake", "fake2"], pool=["fake", "fake2"])
  else:
    cfg, session_blocks, tree = test_task_execution.build_spawning_env(
        tmp_path, monkeypatch, options=backend_options, pool=[option.id for option in backend_options])
  tree.dispatch.executor = test_task_execution._adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  _patch_parent_turn_build(monkeypatch)
  repo = None
  if with_repo:
    repo, _origin = conftest.init_repo_with_origin(tmp_path / "repo")
  manager = await conftest.create_task(tree, parent=None, request_id="root")
  # The launch re-judges the nearest-user authorization gate: the manager
  # carries the real user takeoff message the worker's runs ride on.
  await tree.dispatch.admit_input(manager.id, event_type=ET.USER, content="Take off.", actor="user")
  spec = {"goal": "## Goal\n\nadd a marker file\n", "task_type": task_type}
  if repo is not None:
    spec["repo_path"] = str(repo)
    spec["base_branch"] = "origin/main"
  worker = await conftest.create_task(
      tree, parent=manager.id, request_id="w", profile="worker", task=test_task_execution._task_spec(tree, spec))
  return cfg, tree, manager, worker, repo


async def _save_trigger(cfg, tree, trigger: models.PendingTrigger) -> triggers.TriggerManager:
  mgr = triggers.TriggerManager(cfg, tree)
  await mgr._save_trigger(trigger)
  return mgr


async def _fire(mgr: triggers.TriggerManager, trigger: models.PendingTrigger) -> None:
  with mock.patch(conftest.BROADCAST_PATCH_TARGET, new=mock.AsyncMock()):
    await mgr._wait_and_fire(trigger)


async def _wait_for_new_run(tree, session_id: str, known: set[str], timeout: float = 15.0):
  """Poll until a run record outside *known* exists (a launch is fire-and-forget)."""
  deadline = asyncio.get_event_loop().time() + timeout
  while asyncio.get_event_loop().time() < deadline:
    others = [r for r in tree.runs.list_run_records_sync(session_id) if r.id not in known]
    if others:
      return others[-1]
    await asyncio.sleep(0.05)
  pytest.fail(f"no new run appeared on {session_id} within {timeout}s")


async def _wait_for_review_of(tree, session_id: str, work_run_id: str, timeout: float = 15.0):
  deadline = asyncio.get_event_loop().time() + timeout
  while asyncio.get_event_loop().time() < deadline:
    reviews = [
        r for r in tree.runs.list_run_records_sync(session_id)
        if r.kind == "review" and r.review_of_run_id == work_run_id
    ]
    if reviews:
      return reviews[0]
    await asyncio.sleep(0.05)
  pytest.fail(f"no review of {work_run_id} was registered within {timeout}s")


async def _wait_for_delivery_cleanup(tree, session_id: str, worktree: pathlib.Path, timeout: float = 10.0) -> None:
  """Poll until the node is closed and its worktree removed.

  The finalize chain removes a delivered worktree after the close report is durable, so the
  removal follows the completed state instead of arriving with it.
  """
  deadline = asyncio.get_event_loop().time() + timeout
  while asyncio.get_event_loop().time() < deadline:
    if tree.task_state(session_id) == "completed" and not worktree.exists():
      return
    await asyncio.sleep(0.05)
  pytest.fail(f"node {session_id} was not closed with {worktree} removed within {timeout}s")


def _reviewer_push(tree, worker, run_id: str) -> None:
  """The reviewer's landing work: rebase the worker's branch onto main and push."""
  record = tree.runs.read_run_sync(worker.id, run_id)
  assert record is not None and record.worktree_path and record.branch_name
  work_wt = pathlib.Path(record.worktree_path)
  for args in (("fetch", "-q", "origin"), ("rebase", "-q", "origin/main"), ("push", "-q", "origin",
                                                                            f"{record.branch_name}:main")):
    conftest.run_git(work_wt, *args)


def _launch_prompt(tree, session_id: str, run_id: str) -> str:
  return (tree.runs.run_dir(session_id, run_id) / "launch_prompt.md").read_text(encoding="utf-8")


@pytest.mark.asyncio
async def test_implement_work_run_with_pending_trigger_pauses_then_continues_and_delivers(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, tree, _manager, worker, repo = await _worker_child(tmp_path, monkeypatch)
  builds = install_backends(
      monkeypatch, [
          NativeIdBackend([test_task_execution.result_event("did the work")], native_id="native-1"),
          NativeIdBackend([test_task_execution.result_event("wake handled")], native_id="native-1"),
          test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("review ok")]),
      ])
  # The pending trigger exists before the run starts, so its end provably
  # meets a waiting node.
  mgr = await _save_trigger(cfg, tree, _trigger(worker.id, message="build finished, check the log"))

  await tree.dispatch.admit_input(worker.id, event_type=ET.USER, content="Start the work.", actor="user")
  decision = await tree.dispatch.dispatch_pending(worker.id)
  run1_id = decision["run_id"]
  run1, outcome1 = await test_task_execution.wait_for_terminal_run(tree, worker.id, run1_id)
  assert outcome1 == "success"

  # The pause: no review run, the task stays open, the branch and worktree stay.
  reviews = [r for r in tree.runs.list_run_records_sync(worker.id) if r.kind == "review"]
  assert not reviews, f"the paused run spawned a review: {reviews}"
  assert tree.task_state(worker.id) == "open"
  assert run1.worktree_path and pathlib.Path(run1.worktree_path).is_dir()
  assert run1.branch_name and run1.branch_name in conftest.run_git(repo, "branch", "--list", run1.branch_name)
  # The first run's prompt carries the full context: spec, bindings and batch.
  assert "add a marker file" in _launch_prompt(tree, worker.id, run1_id)

  # The trigger fires: the next run is a work continuation — same
  # conversation, branch and worktree, prompt carrying only the fired input.
  await _fire(mgr, _trigger(worker.id, message="build finished, check the log"))
  run2 = await _wait_for_new_run(tree, worker.id, {run1_id})
  run2, outcome2 = await test_task_execution.wait_for_terminal_run(tree, worker.id, run2.id)
  assert outcome2 == "success"
  assert run2.kind == "work"
  assert run2.worktree_path == run1.worktree_path
  assert run2.branch_name == run1.branch_name
  assert run2.base_branch == run1.base_branch
  assert builds[1]["kwargs"].get("resume_session_id") == "native-1"
  prompt2 = _launch_prompt(tree, worker.id, run2.id)
  assert "build finished, check the log" in prompt2
  assert "add a marker file" not in prompt2
  assert run2.branch_name not in prompt2

  # Ending with nothing pending delivers by the standing rules: the review spawns.
  review = await _wait_for_review_of(tree, worker.id, run2.id)
  assert review is not None


@pytest.mark.asyncio
async def test_input_admitted_during_a_work_run_pauses_it_the_same_way(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  _cfg, tree, manager, worker, _repo = await _worker_child(tmp_path, monkeypatch)
  work_started = asyncio.Event()
  work_gate = asyncio.Event()
  cont_started = asyncio.Event()
  cont_gate = asyncio.Event()
  gate_wait = test_task_execution.signal_then_hold(work_started, work_gate)
  cont_wait = test_task_execution.signal_then_hold(cont_started, cont_gate)

  builds = install_backends(
      monkeypatch, [
          NativeIdBackend([test_task_execution.result_event("did the work")], native_id="native-1", gate=gate_wait),
          NativeIdBackend([test_task_execution.result_event("more done")], native_id="native-1", gate=cont_wait),
          test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("review ok")]),
      ])
  await tree.dispatch.admit_input(worker.id, event_type=ET.USER, content="Start the work.", actor="user")
  decision = await tree.dispatch.dispatch_pending(worker.id)
  run1_id = decision["run_id"]
  await asyncio.wait_for(work_started.wait(), timeout=15)

  # Admitted while the run holds the serialized slot: pending at its end.
  admitted = await tree.dispatch.admit_input(
      worker.id, event_type=ET.AGENT_MESSAGE, content="one more thing", actor="agent", from_session=manager.id)
  work_gate.set()
  run1, outcome1 = await test_task_execution.wait_for_terminal_run(tree, worker.id, run1_id)
  assert outcome1 == "success"

  # The run's own finish dispatches the continuation — no explicit dispatch
  # call. It is parked at its gate, so the pause's shape (no review, open
  # task) is pinned while the continuation provably has not finished.
  await asyncio.wait_for(cont_started.wait(), timeout=15)
  run2 = (await _wait_for_new_run(tree, worker.id, {run1_id}))
  reviews = [r for r in tree.runs.list_run_records_sync(worker.id) if r.kind == "review"]
  assert not reviews, f"the paused run spawned a review: {reviews}"
  assert tree.task_state(worker.id) == "open"
  assert run2.kind == "work"
  assert run2.input_event_ids == [str(admitted["id"])]
  assert run2.worktree_path == run1.worktree_path and run2.branch_name == run1.branch_name
  assert builds[1]["kwargs"].get("resume_session_id") == "native-1"
  prompt2 = _launch_prompt(tree, worker.id, run2.id)
  assert "one more thing" in prompt2
  assert "add a marker file" not in prompt2

  # The continuation ends with nothing pending: the review spawns.
  cont_gate.set()
  _run2, outcome2 = await test_task_execution.wait_for_terminal_run(tree, worker.id, run2.id)
  assert outcome2 == "success"
  review = await _wait_for_review_of(tree, worker.id, run2.id)
  assert review is not None


@pytest.mark.asyncio
async def test_non_implement_work_run_with_pending_trigger_skips_auto_complete(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, tree, _manager, worker, _repo = await _worker_child(tmp_path, monkeypatch, task_type="quick-edit")
  builds = install_backends(
      monkeypatch, [
          NativeIdBackend([test_task_execution.result_event("edit done")], native_id="native-1"),
          NativeIdBackend([test_task_execution.result_event("wake handled")], native_id="native-1"),
      ])
  mgr = await _save_trigger(cfg, tree, _trigger(worker.id))

  await tree.dispatch.admit_input(worker.id, event_type=ET.USER, content="Quick edit.", actor="user")
  decision = await tree.dispatch.dispatch_pending(worker.id)
  run1, outcome1 = await test_task_execution.wait_for_terminal_run(tree, worker.id, decision["run_id"])
  assert outcome1 == "success"

  # No auto-complete, no worktree cleanup: the paused node keeps everything.
  assert tree.task_state(worker.id) == "open"
  closes = [e for e in tree.events.load_events(worker.id) if e["type"] == ET.TASK_CLOSED]
  assert not closes
  assert run1.worktree_path and pathlib.Path(run1.worktree_path).is_dir()

  # The wake's continuation ends with nothing pending: auto-complete delivers.
  await _fire(mgr, _trigger(worker.id))
  run2 = await _wait_for_new_run(tree, worker.id, {run1.id})
  _run2, outcome2 = await test_task_execution.wait_for_terminal_run(tree, worker.id, run2.id)
  assert outcome2 == "success"
  assert builds[1]["kwargs"].get("resume_session_id") == "native-1"
  await _wait_for_delivery_cleanup(tree, worker.id, pathlib.Path(run1.worktree_path))


@pytest.mark.asyncio
async def test_review_with_pending_trigger_pauses_merge_and_continues_the_review(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, tree, manager, worker, repo = await _worker_child(tmp_path, monkeypatch)
  work_started = asyncio.Event()
  work_gate = asyncio.Event()
  review_started = asyncio.Event()
  review_gate = asyncio.Event()
  work_wait = test_task_execution.signal_then_hold(work_started, work_gate)
  review_wait = test_task_execution.signal_then_hold(review_started, review_gate)

  builds = install_backends(
      monkeypatch, [
          NativeIdBackend([test_task_execution.result_event("did the work")], native_id="native-1", gate=work_wait),
          NativeIdBackend(
              [test_task_execution.result_event("review approved")], native_id="native-review-1", gate=review_wait),
          NativeIdBackend(
              [test_task_execution.result_event("merge done")],
              native_id="native-review-1",
              pre_run=lambda: _reviewer_push(tree, worker, run1_id)),
      ])
  await tree.dispatch.admit_input(worker.id, event_type=ET.USER, content="Start the work.", actor="user")
  decision = await tree.dispatch.dispatch_pending(worker.id)
  run1_id = decision["run_id"]
  await asyncio.wait_for(work_started.wait(), timeout=15)
  # The worker's implementation: its commit in the isolated worktree (the
  # fake backend writes no commits itself).
  record = tree.runs.read_run_sync(worker.id, run1_id)
  assert record is not None and record.worktree_path
  work_wt = pathlib.Path(record.worktree_path)
  (work_wt / "marker.txt").write_text("marked\n")
  conftest.run_git(work_wt, "add", "-A")
  conftest.run_git(work_wt, "commit", "-q", "-m", "the work")
  work_gate.set()
  main_before = conftest.run_git(repo, "rev-parse", "origin/main")

  # The review spawns; the trigger lands while it runs.
  await asyncio.wait_for(review_started.wait(), timeout=15)
  review1 = (await _wait_for_review_of(tree, worker.id, run1_id))
  mgr = await _save_trigger(cfg, tree, _trigger(worker.id, message="CI passed"))
  review_gate.set()
  _r1, outcome_r1 = await test_task_execution.wait_for_terminal_run(tree, worker.id, review1.id)
  assert outcome_r1 == "success"

  # The pause: no merge — main is unmoved, the task stays open, nothing reports.
  assert conftest.run_git(repo, "rev-parse", "origin/main") == main_before
  assert tree.task_state(worker.id) == "open"
  reports = conftest.child_reports(tree, manager.id)
  assert not reports, f"the paused review reported before its continuation: {reports}"

  # The wake's next run is a review continuation of the same work run,
  # continuing the reviewer's own conversation.
  await _fire(mgr, _trigger(worker.id, message="CI passed"))
  review2 = await _wait_for_new_run(tree, worker.id, {run1_id, review1.id})
  assert review2.kind == "review" and review2.review_of_run_id == run1_id
  _r2, outcome_r2 = await test_task_execution.wait_for_terminal_run(tree, worker.id, review2.id)
  assert outcome_r2 == "success"
  assert builds[2]["kwargs"].get("resume_session_id") == "native-review-1"
  prompt2 = _launch_prompt(tree, worker.id, review2.id)
  assert "CI passed" in prompt2
  assert "add a marker file" not in prompt2

  # The continuation delivers by the standing rules: merge, close, cleanup, report.
  await _wait_for_delivery_cleanup(tree, worker.id, work_wt)
  assert conftest.run_git(repo, "rev-parse", "origin/main") != main_before
  reports = conftest.child_reports(tree, manager.id)
  assert reports and reports[-1]["outcome"] == "completed"


@pytest.mark.asyncio
async def test_failed_review_continuation_reports_failure_to_the_parent(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, tree, manager, worker, _repo = await _worker_child(tmp_path, monkeypatch)
  work_started = asyncio.Event()
  work_gate = asyncio.Event()
  review_started = asyncio.Event()
  review_gate = asyncio.Event()
  work_wait = test_task_execution.signal_then_hold(work_started, work_gate)
  review_wait = test_task_execution.signal_then_hold(review_started, review_gate)

  install_backends(
      monkeypatch, [
          NativeIdBackend([test_task_execution.result_event("did the work")], native_id="native-1", gate=work_wait),
          NativeIdBackend(
              [test_task_execution.result_event("review approved")], native_id="native-review-1", gate=review_wait),
          NativeIdBackend([], native_id="native-review-1", exit_code=1),
      ])
  await tree.dispatch.admit_input(worker.id, event_type=ET.USER, content="Start the work.", actor="user")
  decision = await tree.dispatch.dispatch_pending(worker.id)
  run1_id = decision["run_id"]
  await asyncio.wait_for(work_started.wait(), timeout=15)
  record = tree.runs.read_run_sync(worker.id, run1_id)
  assert record is not None and record.worktree_path
  work_wt = pathlib.Path(record.worktree_path)
  (work_wt / "marker.txt").write_text("marked\n")
  conftest.run_git(work_wt, "add", "-A")
  conftest.run_git(work_wt, "commit", "-q", "-m", "the work")
  work_gate.set()
  await asyncio.wait_for(review_started.wait(), timeout=15)
  review1 = await _wait_for_review_of(tree, worker.id, run1_id)
  mgr = await _save_trigger(cfg, tree, _trigger(worker.id, message="CI passed"))
  review_gate.set()
  await test_task_execution.wait_for_terminal_run(tree, worker.id, review1.id)

  # The continuation review fails. The paused review's verdict already ended
  # the retry chain, so nothing respawns it: the failure must reach the parent.
  await _fire(mgr, _trigger(worker.id, message="CI passed"))
  review2 = await _wait_for_new_run(tree, worker.id, {run1_id, review1.id})
  assert review2.kind == "review"
  _r2, outcome2 = await test_task_execution.wait_for_terminal_run(tree, worker.id, review2.id)
  assert outcome2 == "failed"
  deadline = asyncio.get_event_loop().time() + 10
  reports: list[dict] = []
  while asyncio.get_event_loop().time() < deadline:
    reports = conftest.child_reports(tree, manager.id)
    if reports:
      break
    await asyncio.sleep(0.1)
  assert reports and reports[-1]["outcome"] == "failed"
  # The failed continuation's report delivery archived the stopped worker.
  await test_task_execution.poll_until(
      lambda: tree.task_state(worker.id) == "archived", timeout=15.0, what="the failed continuation's archive")


@pytest.mark.asyncio
async def test_failed_run_with_pending_trigger_still_reports_failure(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, tree, manager, worker, _repo = await _worker_child(tmp_path, monkeypatch)
  install_backends(monkeypatch, [test_task_execution.SpawningScriptedBackend([], exit_code=1)])
  mgr = await _save_trigger(cfg, tree, _trigger(worker.id))

  await tree.dispatch.admit_input(worker.id, event_type=ET.USER, content="Start the work.", actor="user")
  decision = await tree.dispatch.dispatch_pending(worker.id)
  run1, outcome1 = await test_task_execution.wait_for_terminal_run(tree, worker.id, decision["run_id"])
  assert outcome1 == "failed"

  # Failure reporting is unchanged by the pause: the parent hears it, the
  # trigger stays pending, the task and worktree stay for the explicit retry.
  deadline = asyncio.get_event_loop().time() + 10
  reports: list[dict] = []
  while asyncio.get_event_loop().time() < deadline:
    reports = conftest.child_reports(tree, manager.id)
    if reports:
      break
    await asyncio.sleep(0.1)
  assert reports and reports[-1]["outcome"] == "failed"
  assert tree.task_state(worker.id) == "open"
  assert run1.worktree_path and pathlib.Path(run1.worktree_path).is_dir()
  pending = await mgr.list_triggers(worker.id)
  assert any(t.status == "pending" for t in pending)
  reviews = [r for r in tree.runs.list_run_records_sync(worker.id) if r.kind == "review"]
  assert not reviews


@pytest.mark.asyncio
async def test_changed_prompt_hash_at_continuation_starts_a_fresh_conversation(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:

  _cfg, tree, _manager, worker, _repo = await _worker_child(tmp_path, monkeypatch)
  work_started = asyncio.Event()
  work_gate = asyncio.Event()
  cont_started = asyncio.Event()
  cont_gate = asyncio.Event()
  gate_wait = test_task_execution.signal_then_hold(work_started, work_gate)
  cont_wait = test_task_execution.signal_then_hold(cont_started, cont_gate)

  builds = install_backends(
      monkeypatch, [
          NativeIdBackend([test_task_execution.result_event("did the work")], native_id="native-1", gate=gate_wait),
          NativeIdBackend([test_task_execution.result_event("carried on")], native_id="native-2", gate=cont_wait),
          test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("review ok")]),
      ])
  await tree.dispatch.admit_input(worker.id, event_type=ET.USER, content="Start the work.", actor="user")
  decision = await tree.dispatch.dispatch_pending(worker.id)
  run1_id = decision["run_id"]
  await asyncio.wait_for(work_started.wait(), timeout=15)

  # The input and the instruction change both land during the run: its end is
  # a pause, and the continuation's prompt hash no longer matches.
  await tree.dispatch.admit_input(worker.id, event_type=ET.USER, content="keep going", actor="user")
  await tree.patch_task(worker.id, models.PatchSessionTaskRequest(node_prompt="changed rule"), caller=conftest.OPERATOR)
  work_gate.set()
  run1, outcome1 = await test_task_execution.wait_for_terminal_run(tree, worker.id, run1_id)
  assert outcome1 == "success"

  # The gated continuation pins the fresh-conversation shape before it finishes.
  await asyncio.wait_for(cont_started.wait(), timeout=15)
  run2 = await _wait_for_new_run(tree, worker.id, {run1_id})
  assert run2.kind == "work"
  # Trade-off 1: a fresh conversation — no resume — whose prompt carries the
  # task spec, the new input batch and the previous run's log path, while the
  # branch and worktree are still inherited.
  assert builds[1]["kwargs"].get("resume_session_id") is None
  assert run2.worktree_path == run1.worktree_path and run2.branch_name == run1.branch_name
  prompt2 = _launch_prompt(tree, worker.id, run2.id)
  assert "add a marker file" in prompt2
  assert "keep going" in prompt2
  previous_log = tree.runs.run_dir(worker.id, run1_id) / "events.jsonl"
  assert str(previous_log) in prompt2
  cont_gate.set()
  _run2, outcome2 = await test_task_execution.wait_for_terminal_run(tree, worker.id, run2.id)
  assert outcome2 == "success"


@pytest.mark.asyncio
async def test_delivery_without_pending_wake_runs_the_full_chain(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Regression pin: with no pending trigger and no pending input, the whole
    implement delivery chain runs to the end: review, merge, close."""
  _cfg, tree, manager, worker, _repo = await _worker_child(tmp_path, monkeypatch)
  work_started = asyncio.Event()
  work_gate = asyncio.Event()
  work_wait = test_task_execution.signal_then_hold(work_started, work_gate)

  builds = install_backends(
      monkeypatch, [
          NativeIdBackend([test_task_execution.result_event("did the work")], native_id="native-1", gate=work_wait),
          test_task_execution.SpawningScriptedBackend(
              [test_task_execution.result_event("review ok")], pre_run=lambda: _reviewer_push(tree, worker, run1_id)),
      ])
  await tree.dispatch.admit_input(worker.id, event_type=ET.USER, content="Start the work.", actor="user")
  decision = await tree.dispatch.dispatch_pending(worker.id)
  run1_id = decision["run_id"]
  await asyncio.wait_for(work_started.wait(), timeout=15)
  record = tree.runs.read_run_sync(worker.id, run1_id)
  assert record is not None and record.worktree_path
  work_wt = pathlib.Path(record.worktree_path)
  (work_wt / "marker.txt").write_text("marked\n")
  conftest.run_git(work_wt, "add", "-A")
  conftest.run_git(work_wt, "commit", "-q", "-m", "the work")
  work_gate.set()
  _run1, outcome1 = await test_task_execution.wait_for_terminal_run(tree, worker.id, run1_id)
  assert outcome1 == "success"

  # The review spawns at once, starts a fresh conversation, and its success
  # merges and closes.
  review = await _wait_for_review_of(tree, worker.id, run1_id)
  _review, outcome_r = await test_task_execution.wait_for_terminal_run(tree, worker.id, review.id)
  assert outcome_r == "success"
  assert builds[1]["kwargs"].get("resume_session_id") is None
  await _wait_for_delivery_cleanup(tree, worker.id, work_wt)
  reports = conftest.child_reports(tree, manager.id)
  assert reports and reports[-1]["outcome"] == "completed"


def _two_backends() -> list:
  """A worker backend and a reviewer backend that differ in both id and model."""
  return [
      conftest.backend_option(id="worker-a", label="Worker", type="codex", model="worker-model"),
      conftest.backend_option(id="reviewer-b", label="Reviewer", type="codex", model="reviewer-model"),
  ]


async def _pause_review_with_pending_wake(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, builds_extra: list):
  """Drive a work run on worker-a and its review on reviewer-b, with a wake pending when the review ends.

    Returns (cfg, tree, worker, work run id, review record, the wake's trigger manager, the launch builds)."""
  cfg, tree, _manager, worker, _repo = await _worker_child(tmp_path, monkeypatch, backend_options=_two_backends())
  work_started = asyncio.Event()
  work_gate = asyncio.Event()
  review_started = asyncio.Event()
  review_gate = asyncio.Event()
  work_wait = test_task_execution.signal_then_hold(work_started, work_gate)
  review_wait = test_task_execution.signal_then_hold(review_started, review_gate)

  builds = install_backends(
      monkeypatch, [
          NativeIdBackend([test_task_execution.result_event("did the work")], native_id="native-1", gate=work_wait),
          NativeIdBackend(
              [test_task_execution.result_event("review approved")], native_id="native-review-1", gate=review_wait),
          *builds_extra,
      ])
  await tree.dispatch.admit_input(worker.id, event_type=ET.USER, content="Start the work.", actor="user")
  decision = await tree.dispatch.dispatch_pending(worker.id)
  run1_id = decision["run_id"]
  await asyncio.wait_for(work_started.wait(), timeout=15)
  record = tree.runs.read_run_sync(worker.id, run1_id)
  assert record is not None and record.worktree_path
  work_wt = pathlib.Path(record.worktree_path)
  (work_wt / "marker.txt").write_text("marked\n")
  conftest.run_git(work_wt, "add", "-A")
  conftest.run_git(work_wt, "commit", "-q", "-m", "the work")
  work_gate.set()
  await asyncio.wait_for(review_started.wait(), timeout=15)
  review1 = await _wait_for_review_of(tree, worker.id, run1_id)
  mgr = await _save_trigger(cfg, tree, _trigger(worker.id, message="CI passed"))
  review_gate.set()
  _r1, outcome_r1 = await test_task_execution.wait_for_terminal_run(tree, worker.id, review1.id)
  assert outcome_r1 == "success"
  return cfg, tree, worker, run1_id, review1, mgr, builds


@pytest.mark.asyncio
async def test_review_continuation_runs_on_the_paused_reviews_backend_and_model(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A review continuation keeps the reviewer's backend and model: the worker's model never judges its own work."""
  _cfg, tree, worker, run1_id, review1, mgr, builds = await _pause_review_with_pending_wake(
      tmp_path, monkeypatch, [test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("ok")])])
  assert review1.backend == "reviewer-b"

  await _fire(mgr, _trigger(worker.id, message="CI passed"))
  review2 = await _wait_for_new_run(tree, worker.id, {run1_id, review1.id})
  assert review2.kind == "review" and review2.review_of_run_id == run1_id
  assert (review2.backend, review2.model) == ("reviewer-b", "reviewer-model")
  _r2, outcome_r2 = await test_task_execution.wait_for_terminal_run(tree, worker.id, review2.id)
  assert outcome_r2 == "success"
  assert builds[2]["option"].id == "reviewer-b"


@pytest.mark.asyncio
async def test_review_continuation_refuses_a_reviewer_backend_no_longer_configured(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """No fallback: a paused reviewer whose backend left the config fails its continuation, never the worker's backend."""
  cfg, tree, worker, run1_id, review1, mgr, builds = await _pause_review_with_pending_wake(
      tmp_path, monkeypatch, [test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("ok")])])

  cfg.backends.options = [option for option in cfg.backends.options if option.id != "reviewer-b"]
  await _fire(mgr, _trigger(worker.id, message="CI passed"))
  review2 = await _wait_for_new_run(tree, worker.id, {run1_id, review1.id})
  assert review2.backend == "reviewer-b"
  _r2, outcome_r2 = await test_task_execution.wait_for_terminal_run(tree, worker.id, review2.id)
  assert outcome_r2 == "failed"
  assert len(builds) == 2, "the continuation launched a backend although its reviewer was not configured"


@pytest.mark.asyncio
async def test_work_continuation_keeps_the_node_default_backend_and_model(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  _cfg, tree, _manager, worker, _repo = await _worker_child(tmp_path, monkeypatch, backend_options=_two_backends())
  work_started = asyncio.Event()
  work_gate = asyncio.Event()
  cont_started = asyncio.Event()
  cont_gate = asyncio.Event()
  work_wait = test_task_execution.signal_then_hold(work_started, work_gate)
  cont_wait = test_task_execution.signal_then_hold(cont_started, cont_gate)

  builds = install_backends(
      monkeypatch, [
          NativeIdBackend([test_task_execution.result_event("did the work")], native_id="native-1", gate=work_wait),
          test_task_execution.SpawningScriptedBackend(
              [test_task_execution.result_event("wake handled")], gate=cont_wait),
          test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("review ok")]),
      ])
  await tree.dispatch.admit_input(worker.id, event_type=ET.USER, content="Start the work.", actor="user")
  decision = await tree.dispatch.dispatch_pending(worker.id)
  run1_id = decision["run_id"]
  await asyncio.wait_for(work_started.wait(), timeout=15)
  # Input admitted while the work runs makes its end a pause; the continuation is a work run.
  await tree.dispatch.admit_input(worker.id, event_type=ET.USER, content="keep going", actor="user")
  work_gate.set()
  _run1, outcome1 = await test_task_execution.wait_for_terminal_run(tree, worker.id, run1_id)
  assert outcome1 == "success"

  # Gated, so the continuation is the only new run when the test reads it.
  await asyncio.wait_for(cont_started.wait(), timeout=15)
  run2 = await _wait_for_new_run(tree, worker.id, {run1_id})
  assert run2.kind == "work"
  assert (run2.backend, run2.model) == ("worker-a", "worker-model")
  cont_gate.set()
  _run2, outcome2 = await test_task_execution.wait_for_terminal_run(tree, worker.id, run2.id)
  assert outcome2 == "success"
  assert builds[1]["option"].id == "worker-a"


@pytest.mark.asyncio
async def test_native_session_id_reads_the_claude_system_init_event(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Claude's raw log opens with a system/init event carrying the conversation id; the finalize scan reads it there."""
  claude_option = conftest.backend_option(id="claude", label="Claude", type="cc-claude", model="claude-model")
  cfg, session_blocks, tree = test_task_execution.build_spawning_env(
      tmp_path, monkeypatch, options=[claude_option], pool=["claude"])
  executor = conftest.build_execution_adapter(cfg, session_blocks, tree)
  raw_path = tmp_path / "agent.raw.ndjson"
  init_event = {"type": "system", "subtype": "init", "session_id": "claude-session-1", "model": "claude-model"}
  raw_path.write_text(json.dumps(init_event) + "\n", encoding="utf-8")
  assert await executor._native_session_id(raw_path, claude_option) == "claude-session-1"


@pytest.mark.asyncio
async def test_continuation_on_an_unchanged_backend_names_a_missing_native_id(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  _cfg, tree, _manager, worker, _repo = await _worker_child(tmp_path, monkeypatch)
  work_started = asyncio.Event()
  work_gate = asyncio.Event()
  cont_started = asyncio.Event()
  cont_gate = asyncio.Event()
  work_wait = test_task_execution.signal_then_hold(work_started, work_gate)
  cont_wait = test_task_execution.signal_then_hold(cont_started, cont_gate)

  # The plain double writes no raw log, so its run records no native conversation id.
  builds = install_backends(
      monkeypatch, [
          test_task_execution.SpawningScriptedBackend(
              [test_task_execution.result_event("did the work")], gate=work_wait),
          test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("carried on")], gate=cont_wait),
          test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("review ok")]),
      ])
  await tree.dispatch.admit_input(worker.id, event_type=ET.USER, content="Start the work.", actor="user")
  decision = await tree.dispatch.dispatch_pending(worker.id)
  run1_id = decision["run_id"]
  await asyncio.wait_for(work_started.wait(), timeout=15)
  await tree.dispatch.admit_input(worker.id, event_type=ET.USER, content="keep going", actor="user")
  work_gate.set()
  run1, outcome1 = await test_task_execution.wait_for_terminal_run(tree, worker.id, run1_id)
  assert outcome1 == "success"
  assert run1.native_session_id is None

  await asyncio.wait_for(cont_started.wait(), timeout=15)
  run2 = await _wait_for_new_run(tree, worker.id, {run1_id})
  assert builds[1]["kwargs"].get("resume_session_id") is None
  assert task_execution.NO_NATIVE_ID_RESET_REASON in _launch_prompt(tree, worker.id, run2.id)
  cont_gate.set()
  _run2, outcome2 = await test_task_execution.wait_for_terminal_run(tree, worker.id, run2.id)
  assert outcome2 == "success"
