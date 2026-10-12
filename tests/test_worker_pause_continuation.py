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
import json
from datetime import UTC, datetime
from pathlib import Path
from unittest.mock import AsyncMock, patch

import conftest
import pytest

from src.infra import event_types as ET
from src.infra.models import PendingTrigger
from src.runtime.triggers import TriggerManager
from tests import test_task_execution


class NativeIdBackend(test_task_execution.SpawningScriptedBackend):
  """A scripted backend that leaves the raw transport log's session-attach line behind.

  Real backends tee their raw stdout to ``agent.raw.ndjson``, and the finalize
  path scans that file for the run's native session id; the plain double
  writes no raw file, so a run that must carry a native id (every paused run
  whose continuation should resume its conversation) writes the one raw line
  the scan translates to the attach event.
  """

  def __init__(self, *args, native_id: str, **kwargs) -> None:
    super().__init__(*args, **kwargs)
    self._native_id = native_id
    self.raw_log_dir: str | None = None

  async def run(self, prompt, cwd, env, uploaded_files=None):
    async for event in super().run(prompt, cwd, env, uploaded_files=uploaded_files):
      yield event
    if self.raw_log_dir is not None:
      line = json.dumps({"type": "thread.started", "thread_id": self._native_id}) + "\n"
      (Path(self.raw_log_dir) / "agent.raw.ndjson").write_text(line, encoding="utf-8")


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


def _trigger(session_id: str, trigger_id: str = "trig-1", message: str = "build finished") -> PendingTrigger:
  return PendingTrigger(id=trigger_id, session_id=session_id, fire_at=datetime.now(UTC), message=message)


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
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    *,
    task_type: str = "implement",
    with_repo: bool = True,
):
  """(cfg, tree, manager, worker, repo): a worker child under a manager root with its
    takeoff authorization, the silent-broadcast executor and the parent-turn build double."""
  cfg, session_blocks, tree = test_task_execution.build_env(tmp_path, monkeypatch)
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


async def _save_trigger(cfg, tree, trigger: PendingTrigger) -> TriggerManager:
  mgr = TriggerManager(cfg, tree)
  await mgr._save_trigger(trigger)
  return mgr


async def _fire(mgr: TriggerManager, trigger: PendingTrigger) -> None:
  with patch(conftest.BROADCAST_PATCH_TARGET, new=AsyncMock()):
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


def _launch_prompt(tree, session_id: str, run_id: str) -> str:
  return (tree.runs.run_dir(session_id, run_id) / "launch_prompt.md").read_text(encoding="utf-8")


@pytest.mark.asyncio
async def test_implement_work_run_with_pending_trigger_pauses_then_continues_and_delivers(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
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
  assert run1.worktree_path and Path(run1.worktree_path).is_dir()
  assert run1.branch_name and run1.branch_name in conftest.run_git(repo, "branch", "--list", run1.branch_name)
  # The first run's prompt is today's full context: spec, bindings and batch.
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
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  _cfg, tree, manager, worker, _repo = await _worker_child(tmp_path, monkeypatch)
  work_started = asyncio.Event()
  work_gate = asyncio.Event()
  cont_started = asyncio.Event()
  cont_gate = asyncio.Event()

  async def gate_wait() -> None:
    work_started.set()
    await work_gate.wait()

  async def cont_wait() -> None:
    cont_started.set()
    await cont_gate.wait()

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

  # The continuation ends with nothing pending: the review spawns as today.
  cont_gate.set()
  _run2, outcome2 = await test_task_execution.wait_for_terminal_run(tree, worker.id, run2.id)
  assert outcome2 == "success"
  review = await _wait_for_review_of(tree, worker.id, run2.id)
  assert review is not None


@pytest.mark.asyncio
async def test_non_implement_work_run_with_pending_trigger_skips_auto_complete(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
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
  assert run1.worktree_path and Path(run1.worktree_path).is_dir()

  # The wake's continuation ends with nothing pending: auto-complete delivers.
  await _fire(mgr, _trigger(worker.id))
  run2 = await _wait_for_new_run(tree, worker.id, {run1.id})
  _run2, outcome2 = await test_task_execution.wait_for_terminal_run(tree, worker.id, run2.id)
  assert outcome2 == "success"
  assert builds[1]["kwargs"].get("resume_session_id") == "native-1"
  deadline = asyncio.get_event_loop().time() + 10
  while asyncio.get_event_loop().time() < deadline:
    if tree.task_state(worker.id) == "completed":
      break
    await asyncio.sleep(0.1)
  assert tree.task_state(worker.id) == "completed"
  assert not Path(run1.worktree_path).exists()


@pytest.mark.asyncio
async def test_review_with_pending_trigger_pauses_merge_and_continues_the_review(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, tree, manager, worker, repo = await _worker_child(tmp_path, monkeypatch)
  work_started = asyncio.Event()
  work_gate = asyncio.Event()
  review_started = asyncio.Event()
  review_gate = asyncio.Event()

  async def work_wait() -> None:
    work_started.set()
    await work_gate.wait()

  async def review_wait() -> None:
    review_started.set()
    await review_gate.wait()

  def reviewer_push() -> None:
    """The reviewer's landing work: rebase the branch onto the target and push."""
    record = tree.runs.read_run_sync(worker.id, run1_id)
    assert record is not None and record.worktree_path and record.branch_name
    work_wt = Path(record.worktree_path)
    for args in (("fetch", "-q", "origin"), ("rebase", "-q", "origin/main"), ("push", "-q", "origin",
                                                                              f"{record.branch_name}:main")):
      conftest.run_git(work_wt, *args)

  builds = install_backends(
      monkeypatch, [
          NativeIdBackend([test_task_execution.result_event("did the work")], native_id="native-1", gate=work_wait),
          NativeIdBackend(
              [test_task_execution.result_event("review approved")], native_id="native-review-1", gate=review_wait),
          NativeIdBackend(
              [test_task_execution.result_event("merge done")], native_id="native-review-1", pre_run=reviewer_push),
      ])
  await tree.dispatch.admit_input(worker.id, event_type=ET.USER, content="Start the work.", actor="user")
  decision = await tree.dispatch.dispatch_pending(worker.id)
  run1_id = decision["run_id"]
  await asyncio.wait_for(work_started.wait(), timeout=15)
  # The worker's implementation: its commit in the isolated worktree (the
  # fake backend writes no commits itself).
  record = tree.runs.read_run_sync(worker.id, run1_id)
  assert record is not None and record.worktree_path
  work_wt = Path(record.worktree_path)
  (work_wt / "marker.txt").write_text("marked\n")
  conftest.run_git(work_wt, "add", "-A")
  conftest.run_git(work_wt, "commit", "-q", "-m", "the work")
  work_gate.set()
  main_before = conftest.run_git(repo, "rev-parse", "origin/main")

  # The review spawns as today; the trigger lands while it runs.
  await asyncio.wait_for(review_started.wait(), timeout=15)
  review1 = (await _wait_for_review_of(tree, worker.id, run1_id))
  mgr = await _save_trigger(cfg, tree, _trigger(worker.id, message="CI passed"))
  review_gate.set()
  _r1, outcome_r1 = await test_task_execution.wait_for_terminal_run(tree, worker.id, review1.id)
  assert outcome_r1 == "success"

  # The pause: no merge — main is unmoved, the task stays open, nothing reports.
  assert conftest.run_git(repo, "rev-parse", "origin/main") == main_before
  assert tree.task_state(worker.id) == "open"
  reports = [e for e in tree.events.load_events(manager.id) if e["type"] == ET.CHILD_REPORT]
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
  deadline = asyncio.get_event_loop().time() + 10
  while asyncio.get_event_loop().time() < deadline:
    if tree.task_state(worker.id) == "completed":
      break
    await asyncio.sleep(0.1)
  assert tree.task_state(worker.id) == "completed"
  assert conftest.run_git(repo, "rev-parse", "origin/main") != main_before
  assert not Path(record.worktree_path).exists()
  reports = [e for e in tree.events.load_events(manager.id) if e["type"] == ET.CHILD_REPORT]
  assert reports and reports[-1]["outcome"] == "completed"


@pytest.mark.asyncio
async def test_failed_run_with_pending_trigger_reports_failure_as_today(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
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
    reports = [e for e in tree.events.load_events(manager.id) if e.get("type") == ET.CHILD_REPORT]
    if reports:
      break
    await asyncio.sleep(0.1)
  assert reports and reports[-1]["outcome"] == "failed"
  assert tree.task_state(worker.id) == "open"
  assert run1.worktree_path and Path(run1.worktree_path).is_dir()
  pending = await mgr.list_triggers(worker.id)
  assert any(t.status == "pending" for t in pending)
  reviews = [r for r in tree.runs.list_run_records_sync(worker.id) if r.kind == "review"]
  assert not reviews


@pytest.mark.asyncio
async def test_changed_prompt_hash_at_continuation_starts_a_fresh_conversation(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  from src.infra.models import PatchSessionTaskRequest

  _cfg, tree, _manager, worker, _repo = await _worker_child(tmp_path, monkeypatch)
  work_started = asyncio.Event()
  work_gate = asyncio.Event()
  cont_started = asyncio.Event()
  cont_gate = asyncio.Event()

  async def gate_wait() -> None:
    work_started.set()
    await work_gate.wait()

  async def cont_wait() -> None:
    cont_started.set()
    await cont_gate.wait()

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
  await tree.patch_task(worker.id, PatchSessionTaskRequest(node_prompt="changed rule"), caller=conftest.OPERATOR)
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
async def test_delivery_without_pending_wake_behaves_as_today(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Regression pin: with no pending trigger and no pending input, the whole
    implement delivery chain runs exactly as before the pause existed."""
  _cfg, tree, manager, worker, _repo = await _worker_child(tmp_path, monkeypatch)
  work_started = asyncio.Event()
  work_gate = asyncio.Event()

  async def work_wait() -> None:
    work_started.set()
    await work_gate.wait()

  def reviewer_push() -> None:
    record = tree.runs.read_run_sync(worker.id, run1_id)
    assert record is not None and record.worktree_path and record.branch_name
    work_wt = Path(record.worktree_path)
    for args in (("fetch", "-q", "origin"), ("rebase", "-q", "origin/main"), ("push", "-q", "origin",
                                                                              f"{record.branch_name}:main")):
      conftest.run_git(work_wt, *args)

  builds = install_backends(
      monkeypatch, [
          NativeIdBackend([test_task_execution.result_event("did the work")], native_id="native-1", gate=work_wait),
          test_task_execution.SpawningScriptedBackend(
              [test_task_execution.result_event("review ok")], pre_run=reviewer_push),
      ])
  await tree.dispatch.admit_input(worker.id, event_type=ET.USER, content="Start the work.", actor="user")
  decision = await tree.dispatch.dispatch_pending(worker.id)
  run1_id = decision["run_id"]
  await asyncio.wait_for(work_started.wait(), timeout=15)
  record = tree.runs.read_run_sync(worker.id, run1_id)
  assert record is not None and record.worktree_path
  work_wt = Path(record.worktree_path)
  (work_wt / "marker.txt").write_text("marked\n")
  conftest.run_git(work_wt, "add", "-A")
  conftest.run_git(work_wt, "commit", "-q", "-m", "the work")
  work_gate.set()
  _run1, outcome1 = await test_task_execution.wait_for_terminal_run(tree, worker.id, run1_id)
  assert outcome1 == "success"

  # The review spawns at once, starts a fresh conversation, and its success
  # merges and closes exactly as today.
  review = await _wait_for_review_of(tree, worker.id, run1_id)
  _review, outcome_r = await test_task_execution.wait_for_terminal_run(tree, worker.id, review.id)
  assert outcome_r == "success"
  assert builds[1]["kwargs"].get("resume_session_id") is None
  deadline = asyncio.get_event_loop().time() + 10
  while asyncio.get_event_loop().time() < deadline:
    if tree.task_state(worker.id) == "completed":
      break
    await asyncio.sleep(0.1)
  assert tree.task_state(worker.id) == "completed"
  assert not Path(record.worktree_path).exists()
  reports = [e for e in tree.events.load_events(manager.id) if e["type"] == ET.CHILD_REPORT]
  assert reports and reports[-1]["outcome"] == "completed"
