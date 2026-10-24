"""Inputs admitted during a review Run must reach the node's own dispatcher.

Each test pins one admit-during-review shape: the input lands while the review
Run holds the node's serialized slot, and its dispatch decision comes from the
node's own dispatcher only after the review chain finishes.
"""

from __future__ import annotations

import asyncio
import pathlib

import conftest
import pytest

from src.infra import event_types as ET
from src.infra import git, models
from tests import test_task_execution


@pytest.mark.asyncio
async def test_input_admitted_during_a_review_pauses_the_merge_and_continues_the_review(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """An input admitted while the review Run holds the serialized slot pauses
    the review's delivery and rides the review's own continuation.

    Deterministic by construction: the review backend is GATED, so the admit
    and the landing-defeating commit provably land during the review — no race
    against the scripted review's finish. The review itself succeeds; the
    pending input makes its ending a pause: no landing check, no merge, no
    report. The continuation is a review Run (the latest Run is a successful
    review whose merge never started), gated so the child-open assertion runs
    while it provably has not finished. Only the continuation's own finish
    runs the landing check — and there the empty commit left on the branch
    during the first review makes it honestly fail, so the chain reports
    "review run ... ended without landing work run".
    """
  cfg, session_blocks, tree = test_task_execution.build_env(
      tmp_path, monkeypatch, backend_ids=["fake", "fake2"], pool=["fake", "fake2"])
  tree.dispatch.executor = test_task_execution._adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  repo, _origin = conftest.init_repo_with_origin(tmp_path / "repo")
  manager = await conftest.create_task(
      tree, parent=None, request_id="root", profile="manager", task=models.TaskSpec(goal="pm"), name="PM")
  conftest.install_scripted_backends(
      monkeypatch,
      [
          test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("taken off")]),
          # The blocked-delivery report wakes the parent's own turn...
          test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("blocked report noted")]),
          # ...and so does the delivery report when the tweak's chain closes
          # the child.
          test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("delivery report noted")]),
      ],
      conftest.BUILD_BACKEND_PATCH_TARGET)
  await tree.dispatch.admit_input(manager.id, event_type=ET.USER, content="Take off.", actor="user")
  decision = await tree.dispatch.dispatch_pending(manager.id)
  await test_task_execution.wait_for_terminal_run(tree, manager.id, decision["run_id"])

  child = await tree.create_task(
      request_id="child",
      task_parent_id=manager.id,
      profile="worker",
      task=models.TaskSpec(goal="do the work", repo_path=str(repo), task_type=models.TaskType.IMPLEMENT),
      name="W",
      backend=None,
      caller="operator")
  review_started = asyncio.Event()
  review_gate = asyncio.Event()
  tweak_started = asyncio.Event()
  tweak_gate = asyncio.Event()
  review_gate_wait = test_task_execution.signal_then_hold(review_started, review_gate)
  tweak_gate_wait = test_task_execution.signal_then_hold(tweak_started, tweak_gate)

  conftest.install_scripted_backends(
      monkeypatch,
      [
          test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("work done")]),
          # The gated review: it cannot finish before the test's mid-review
          # actions below, so they are provably "during the review Run".
          test_task_execution.SpawningScriptedBackend(
              [test_task_execution.result_event("review approved")], gate=review_gate_wait),
          # The stranded input's review continuation, gated so the child-open
          # assertion below runs while it provably has not finished.
          test_task_execution.SpawningScriptedBackend(
              [test_task_execution.result_event("tweak reviewed")], gate=tweak_gate_wait),
      ],
      conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)
  await tree.dispatch.admit_input(child.id, event_type=ET.USER, content="Start the work.", actor="user")
  d1 = await tree.dispatch.dispatch_pending(child.id)
  work_run_id = d1["run_id"]
  await test_task_execution.wait_for_terminal_run(tree, child.id, work_run_id)

  # The work Run's terminal fact lands before its delivery chain registers the
  # review Run; the gated review backend starting is the event that proves the
  # review Run is registered and holds the node's serialized slot.
  await asyncio.wait_for(review_started.wait(), timeout=15)
  review_runs = [r for r in tree.runs.list_run_records_sync(child.id) if r.kind == "review"]
  assert len(review_runs) == 1
  review_run = review_runs[0]
  # The gated review cannot finish before the gate below releases, so every
  # action until then is provably "during the review Run".
  assert tree.runs.terminal_outcome(tree.runs.load_events_sync(child.id), review_run.id) is None

  # DURING the review: admit the input and leave a commit on the work branch
  # that never lands on main, so the continuation's landing check honestly
  # fails when it finally runs. The input rides the parent manager's
  # provenance, the request-entry gate's authorized sender.
  await tree.dispatch.admit_input(
      child.id, event_type=ET.AGENT_MESSAGE, content="one more tweak", actor="agent", from_session=manager.id)
  work_run = await tree.runs.get_run(child.id, work_run_id)
  assert work_run is not None and work_run.worktree_path
  ok, _out, _err = await git._git_stdout(
      pathlib.Path(work_run.worktree_path),
      "commit",
      "--allow-empty",
      "-q",
      "-m",
      "unlanded wip",
      timeout=10,
      timeout_label="unlanded-wip")
  assert ok
  review_gate.set()

  # The successful review's ending was a pause: no landing check ran, so no
  # blocked report reached the parent yet, and the child stays open.
  await test_task_execution.wait_for_terminal_run(tree, child.id, review_run.id, timeout=10.0)
  assert tree.task_state(child.id) == "open"
  reports = conftest.child_reports(tree, manager.id)
  assert not reports, f"the paused review reported before its continuation: {reports}"

  # The stranded input's next permitted serialized dispatch is the review's
  # continuation: a review Run of the same work Run, parked at its gate, so it
  # provably has not finished — the point where the contract pins the child
  # open. Reading the state earlier or later races the delivery chain.
  await asyncio.wait_for(tweak_started.wait(), timeout=15)
  records = tree.runs.list_run_records_sync(child.id)
  next_runs = [r for r in records if r.id not in (work_run_id, review_run.id)]
  assert len(next_runs) == 1, f"the input admitted during the review was stranded: {records}"
  assert next_runs[0].kind == "review" and next_runs[0].review_of_run_id == work_run_id
  assert next_runs[0].input_event_ids, "the next dispatch did not claim the admitted input"
  assert tree.task_state(child.id) == "open"
  tweak_gate.set()

  # Released, the continuation finishes successfully — and only now does the
  # landing check run: the unlanded commit defeats it, so the chain reports
  # "review run ... ended without landing work run" and the report's delivery
  # archives the child.
  _tweak_record, tweak_outcome = await test_task_execution.wait_for_terminal_run(
      tree, child.id, next_runs[0].id, timeout=10.0)
  assert tweak_outcome == "success"
  deadline = asyncio.get_event_loop().time() + 15
  while asyncio.get_event_loop().time() < deadline:
    reports = conftest.child_reports(tree, manager.id)
    if any("review" in str(e.get("summary", "")).lower() for e in reports) and tree.task_state(child.id) == "archived":
      break
    await asyncio.sleep(0.1)
  assert any("review" in str(e.get("summary", "")).lower() for e in reports), reports
  assert tree.task_state(child.id) == "archived"
