"""Improve-sequence tests: the v2 improve loop is one worker child with
iteration Runs, one final report, and no automatic whole-loop restart.

Exercises the actual /api/internal/improve entry point against synthetic
instances with deterministic scripted backend processes: two iterations stay
one child, a live goal change steers the next iteration, the success/stop and
quota outcomes are truthful, and a restart marks the interrupted controller
honestly without resuming the loop or leaving a permanently blocking lock.
"""

from __future__ import annotations

import asyncio
import json
import os
import subprocess
from datetime import UTC, datetime
from pathlib import Path

import pytest
from conftest import (
    FABLE_MODEL,
    OPERATOR,
    WORKER_BUILD_BACKEND_PATCH_TARGET,
    _async_wait_for,
    create_task,
    identity_of,
    install_scripted_backends,
    rate_limit_event,
    stub_credentials,
)

from src.backends.claude_code import claude_accounts, claude_relay
from src.features.improve import improve_command, improve_sequence
from src.features.improve.improve_command import ImproveState, load_loop_state, save_loop_state
from src.infra import event_types as ET
from src.infra.models import RunRecord, SequenceRef, SessionMetadata, TaskSpec
from src.infra.tasks import create_logged_task
from src.runtime.runs import RUN_EVENTS_NAME
from src.runtime.task_sessions import TaskTreeManager
from tests.test_task_execution import (
    BUILD_BACKEND_PATCH_TARGET,
    OP_HEADERS,
    SpawningScriptedBackend,
    _adapter_with_silent_broadcast,
    build_env,
    build_pooled_env,
    install_worker_launch_and_resume_backends,
    make_api_client,
    make_pm_build,
    result_event,
    write_raw_result,
)


def _worker_backends(monkeypatch, outcomes: list[str]) -> list:
  """One scripted worker per iteration build, in launch order."""
  return install_scripted_backends(
      monkeypatch, [SpawningScriptedBackend([result_event(text)]) for text in outcomes],
      WORKER_BUILD_BACKEND_PATCH_TARGET)


def _close_facts(tree, child_id: str) -> list[dict]:
  """The child's task_closed facts (the loop's close step writes at most one)."""
  return [e for e in tree.events.load_events(child_id) if e["type"] == ET.TASK_CLOSED]


def _assert_single_close(tree, child_id: str, *, lifecycle: str, sequence_outcome: str) -> dict:
  """The one close fact the loop's close step landed, with both outcome fields."""
  closes = _close_facts(tree, child_id)
  assert len(closes) == 1
  assert closes[0]["request_id"] == "improve:1:close"
  assert closes[0]["outcome"] == lifecycle
  assert closes[0]["sequence_outcome"] == sequence_outcome
  return closes[0]


async def _start_loop_in_process(
    cfg,
    tree,
    manager,
    repo: Path,
    *,
    iterations: int = 2,
    merge_back: bool = False,
) -> tuple[dict, str, asyncio.Task]:
  """Reserve and spawn the improve controller in this process (the API
  handler's own steps), so the test drives gates on its own event loop."""
  state = await improve_command.reserve_loop_state(
      manager.id,
      "## Goal\n\nimprove the thing\n",
      "improve/test-branch",
      str(repo),
      cfg,
      base_branch="main",
      merge_back=merge_back,
      resolved_backend="fake",
      resolved_model="fake-model")
  child = await improve_sequence.create_improve_child(
      tree, manager.id, state.loop_id, "improve the thing", repo_path=str(repo), base_branch="main")
  task = create_logged_task(
      improve_sequence.run_improve_sequence(
          manager.id,
          cfg,
          tree,
          loop_id=state.loop_id,
          iterations=iterations,
          child_id=child.id,
          goal="improve the thing"),
      name=f"improve-sequence-test-{state.loop_id}")
  return {"loop_id": state.loop_id, "child_session_id": child.id}, child.id, task


async def _ended_loop_shape(
    cfg,
    tree,
    manager,
    *,
    status: str,
    iterations: list[tuple[str | None, str | None]],
    server_pid: int | None = None,
) -> str:
  """One improve loop's durable after-crash shape: the worker child, the loop
  state, and each iteration as (terminal outcome or None for still-queued,
  judged report text or None). Returns the child id."""
  loop_id = 1
  child = await improve_sequence.create_improve_child(
      tree, manager.id, loop_id, "improve the thing", repo_path=None, base_branch=None)
  await save_loop_state(
      manager.id,
      ImproveState(
          loop_id=loop_id,
          goal="improve the thing",
          status=status,
          work_branch="improve/test",
          base_branch="main",
          repo_path=str(cfg.charliebot_home),
          created_at="2026-10-08T00:00:00+00:00",
          server_pid=os.getpid() if server_pid is None else server_pid,
      ), cfg)
  loop_dir = cfg.sessions_dir / manager.id / "loops" / str(loop_id)
  for position, (outcome, report_text) in enumerate(iterations, start=1):
    run = RunRecord(
        id=f"iter-run-{position}",
        session_id=child.id,
        kind="iteration",
        backend="fake",
        model="fake-model",
        sequence_ref=SequenceRef(
            kind="improve", owner_ref=improve_sequence.loop_owner_ref(manager.id, loop_id, cfg), position=position))
    await tree.runs.register_run(run)
    if outcome is not None:
      await tree.runs.record_finish(child.id, run.id, outcome)
    if report_text is not None:
      (loop_dir / f"iter_{position:04d}.md").write_text(report_text)
  return child.id


async def _admit_takeoff(tree: TaskTreeManager, manager: SessionMetadata) -> None:
  """The operator's first user message; its dispatch runs the manager's takeoff turn."""
  await tree.dispatch.admit_input(
      manager.id, event_type=ET.USER, content="Take off. Run the improve loop.", actor="user")


async def _start_loop(
    cfg, session_blocks, tree, manager, repo: Path, monkeypatch, payload_overrides=None, wait_effect=None):
  """POST the improve loop against the v2 manager and wait for the controller's child."""
  stub_credentials({"charliebot": {"access_key": "op-secret"}})
  monkeypatch.setenv("CHARLIEBOT_HOME", str(cfg.charliebot_home))
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  # The final report's wake dispatches the manager's report-consuming turn;
  # script it through the registry builder so no external process starts.
  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, make_pm_build("report noted"))
  payload = {
      "session_id": manager.id,
      "goal": "## Goal\n\nimprove the thing\n",
      "iterations": 2,
      "repo_path": str(repo),
      "base_branch": "main",
      "work_branch": "improve/test-branch",
  }
  payload.update(payload_overrides or {})
  with make_api_client(cfg, session_blocks, tree) as client:
    resp = client.post("/api/internal/improve", json=payload, headers=OP_HEADERS)
    assert resp.status_code == 200, resp.text
    body = resp.json()
    assert body["status"] == "started"
    child_id = body["child_session_id"]
    # The controller task is born inside the client's request loop; every
    # wait below happens while that loop is still alive.
    if wait_effect is not None:
      await wait_effect(client, body)
    else:
      deadline = asyncio.get_event_loop().time() + 10
      while asyncio.get_event_loop().time() < deadline:
        meta = await tree.load_meta(child_id)
        if meta is not None and tree.runs.list_run_records_sync(child_id):
          break
        await asyncio.sleep(0.05)
      else:
        pytest.fail("the sequence controller never registered its first iteration run")
  return body, child_id


@pytest.mark.asyncio
async def test_two_iterations_stay_one_child_with_ordered_runs(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
  """Two iterations: ONE child, two ordered iteration Runs, one final report."""
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  manager = await create_task(
      tree, parent=None, request_id="root", profile="manager", task=TaskSpec(goal="pm"), name="PM")
  builds = _worker_backends(monkeypatch, ["iter one words", "iter two words"])

  await _admit_takeoff(tree, manager)

  async def _wait_done(client, body):
    await _wait_for_iterations_settled(tree, body["child_session_id"], manager.id, timeout=30.0)

  body, child_id = await _start_loop(cfg, session_blocks, tree, manager, repo, monkeypatch, wait_effect=_wait_done)

  records = tree.runs.list_run_records_sync(child_id)
  assert [r.kind for r in records] == ["iteration", "iteration"]
  # Every launched iteration carries the assembler's durable snapshot evidence.
  for r in records:
    assert r.prompt_snapshot_ref is not None and Path(r.prompt_snapshot_ref).is_file()
    stored = json.loads(Path(r.prompt_snapshot_ref).read_text(encoding="utf-8"))
    refs = [s["source_ref"] for b in stored["blocks"] for s in b["sources"]]
    assert "prompts/worker.md" in refs  # the iteration's applicable contract
  positions = [r.sequence_ref.position for r in records]
  kinds = [r.sequence_ref.kind for r in records]
  assert positions == [1, 2] and kinds == ["improve", "improve"]
  assert all(str(repo) in r.sequence_ref.owner_ref for r in records) or all(
      "loops" in r.sequence_ref.owner_ref for r in records)
  # ONE child only: the loop never created a second task under the manager.
  metas = [
      SessionMetadata.model_validate_json(p.read_text()) for p in sorted((cfg.sessions_dir).glob("*/metadata.json"))
  ]
  assert [m.id for m in metas if m.task_parent_id == manager.id] == [child_id]
  # Both iteration prompts carry the worker memory block + iteration report
  # instructions (the existing worker prompt builder, loop_dir context).
  assert len(builds) == 2
  for i, b in enumerate(builds, start=1):
    assert f"iter_{i:04d}.md" in b["backend"].prompt
  # One final result report on the manager, from the child, after BOTH runs,
  # plus one per-iteration report for each judged iteration.
  report = await _wait_for_final_report(tree, manager.id, timeout=10.0)
  assert report.get("child_session_id") == child_id
  assert report.get("outcome") in ("completed", "blocked", "failed", "cancelled")
  assert "Improve loop" in str(report.get("summary"))
  assert len(_iteration_reports(tree, manager.id)) == 2
  # The loop state is honestly 'blocked' — iterations exhausted without a
  # proven landing (no merge-back) is NOT successful delivery.
  state = await load_loop_state(manager.id, body["loop_id"], cfg)
  assert state is not None and state.status == "blocked"
  # The loop's end closed the child through the completion owner: lifecycle
  # cancelled with the loop's own outcome on the close fact, and the listing
  # counts the node as archived.
  assert tree.task_state(child_id) == "cancelled"
  index = await tree._get_index()
  assert tree.archived_of(index, index.metas[child_id])
  _assert_single_close(tree, child_id, lifecycle="cancelled", sequence_outcome="blocked")
  assert len(_final_reports(tree, manager.id)) == 1 and report["outcome"] == "blocked"
  assert report["summary"].startswith("[Improve loop 1] Improve loop ran 2 iteration(s)")
  assert "Iteration summaries:" in report["summary"] and "iter two words" in report["summary"]
  assert report["result_refs"] == ["loop:1"]


@pytest.mark.asyncio
async def test_live_goal_change_affects_next_iteration(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
  """The controller re-reads goal.md every iteration: a mid-loop edit steers the next one."""
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  manager = await create_task(
      tree, parent=None, request_id="root", profile="manager", task=TaskSpec(goal="pm"), name="PM")

  async def edit_goal_after_first_stream() -> None:
    # After the first build's scripted stream, edit the live goal so the
    # second iteration must see it.
    goal_path = cfg.sessions_dir / manager.id / "loops" / "1" / "goal.md"
    goal_path.write_text("## Goal\n\nnow improve the OTHER thing\n")

  first = SpawningScriptedBackend([result_event("one")], post_events=edit_goal_after_first_stream)
  second = SpawningScriptedBackend([result_event("two")])
  queue = [first, second]
  monkeypatch.setattr(WORKER_BUILD_BACKEND_PATCH_TARGET, lambda *a, **k: queue.pop(0))
  stub_credentials({"charliebot": {"access_key": "op-secret"}})
  monkeypatch.setenv("CHARLIEBOT_HOME", str(cfg.charliebot_home))
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  # The final report's wake dispatches the manager's report-consuming turn;
  # script it through the registry builder so no external process starts.
  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, make_pm_build("report noted"))

  await _admit_takeoff(tree, manager)
  with make_api_client(cfg, session_blocks, tree) as client:
    resp = client.post(
        "/api/internal/improve",
        json={
            "session_id": manager.id,
            "goal": "## Goal\n\nimprove the thing\n",
            "iterations": 2,
            "repo_path": str(repo),
            "base_branch": "main",
            "work_branch": "improve/live-goal",
        },
        headers=OP_HEADERS)
    assert resp.status_code == 200, resp.text
    child_id = resp.json()["child_session_id"]

    await _wait_for_iterations_settled(tree, child_id, manager.id, timeout=30.0)
  # The second build's prompt carries the EDITED goal, not the original one.
  assert second.prompt is not None
  assert "now improve the OTHER thing" in second.prompt
  assert "improve the thing" not in second.prompt.split("Previous iteration summaries")[0]


@pytest.mark.asyncio
async def test_improve_without_authorization_is_forbidden_not_a_server_error(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
  """No take-off anywhere in the chain: 403 with the gate's reason, never a
  500, and nothing reserved."""
  from src.features.improve.improve_command import _active_loop_path, _loops_dir, find_running_loop
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  manager = await create_task(
      tree, parent=None, request_id="root", profile="manager", task=TaskSpec(goal="pm"), name="PM")
  stub_credentials({"charliebot": {"access_key": "op-secret"}})
  monkeypatch.setenv("CHARLIEBOT_HOME", str(cfg.charliebot_home))
  payload = {
      "session_id": manager.id,
      "goal": "## Goal\n\nimprove the thing\n",
      "iterations": 1,
      "repo_path": str(repo),
      "base_branch": "main",
  }
  with make_api_client(cfg, session_blocks, tree) as client:
    resp = client.post("/api/internal/improve", json=payload, headers=OP_HEADERS)
  assert resp.status_code == 403, resp.text
  assert "take off" in str(resp.json()["detail"]).lower() or "authorization" in str(resp.json()["detail"]).lower()
  assert not _active_loop_path(manager.id, cfg).exists()
  assert await find_running_loop(manager.id, cfg) is None
  loops_dir = _loops_dir(manager.id, cfg)
  assert not loops_dir.is_dir() or list(loops_dir.glob("*")) == []


# ---------------------------------------------------------------------------
# Pooled launches: iteration Runs start on a Claude pool account
# ---------------------------------------------------------------------------


def _child_reports(tree, manager_id: str) -> list[dict]:
  """Every child report on the manager, in delivery order."""
  return [e for e in tree.events.load_events(manager_id) if e.get("type") == ET.CHILD_REPORT]


def _iteration_reports(tree, manager_id: str) -> list[dict]:
  """The per-iteration child reports: their header names the iteration."""
  return [e for e in _child_reports(tree, manager_id) if "· iteration" in str(e.get("summary"))]


def _final_reports(tree, manager_id: str) -> list[dict]:
  """The loop's final child reports: the "[Improve loop <id>]" prefix without
  the iteration header the per-iteration reports carry (a failed iteration
  Run has no adapter failure report: the adapter's delivery chain skips
  iteration Runs, so the loop's reports are the only ones on the manager)."""
  return [
      e for e in _child_reports(tree, manager_id)
      if str(e.get("summary")).startswith("[Improve loop") and "· iteration" not in str(e.get("summary"))
  ]


async def _wait_for_iterations_settled(tree, child_id: str, manager_id: str, timeout: float = 30.0) -> None:
  """Both iteration Runs terminal and the loop's final report on the manager."""

  def _settled() -> bool:
    records = tree.runs.list_run_records_sync(child_id)
    return bool(
        len(records) == 2 and _final_reports(tree, manager_id) and
        all(tree.runs.terminal_outcome(tree.runs.load_events_sync(child_id), r.id) is not None for r in records))

  await _async_wait_for(_settled, timeout, "both iterations never finished")


async def _wait_for_final_report(tree, manager_id: str, timeout: float = 30.0) -> dict:
  """The ONE final sequence result on the manager, from the child."""
  deadline = asyncio.get_event_loop().time() + timeout
  while asyncio.get_event_loop().time() < deadline:
    final = _final_reports(tree, manager_id)
    if final:
      return final[0]
    await asyncio.sleep(0.1)
  pytest.fail(f"the final sequence result was never delivered: {_child_reports(tree, manager_id)}")


def _count_manager_wakes(monkeypatch: pytest.MonkeyPatch, tree, manager_id: str) -> list[str]:
  """Record every parent wake the manager receives, without launching its turn.

  The wake is the behavior under test; the stubbed dispatch keeps a doomed
  manager turn (no scripted parent backend here) from churning behind it.
  """
  wakes: list[str] = []

  async def stub_dispatch(session_id: str) -> dict:
    if session_id == manager_id:
      wakes.append(session_id)
    return {"session_id": session_id, "pending": 0, "launch": False}

  monkeypatch.setattr(tree.dispatch, "dispatch_pending", stub_dispatch)
  return wakes


async def _pooled_pm_manager(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> tuple:
  """One pooled-account trial stage: the account registry reset, the pooled env,
  and its root PM manager node. The pooled-account tests script their own
  backends and wake recording on top of this stage."""
  claude_accounts.reset_for_tests()
  cfg, session_blocks, tree = build_pooled_env(tmp_path, monkeypatch)
  manager = await tree.create_task(
      request_id="root",
      task_parent_id=None,
      profile="manager",
      task=TaskSpec(goal="pm"),
      name="PM",
      backend=None,
      caller=OPERATOR)
  return cfg, session_blocks, tree, manager


@pytest.mark.asyncio
async def test_pooled_iteration_launches_on_the_selected_pool_account(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
  """The improve controller's iteration Runs are fresh worker launches: with a
  non-empty pool and a cc-claude backend, each lifecycle selects the account
  claude_accounts.select returned, and the relay loop is armed (the backend
  build receives the same account)."""
  cfg, session_blocks, tree, manager = await _pooled_pm_manager(tmp_path, monkeypatch)
  builds = _worker_backends(monkeypatch, ["iter one words", "iter two words"])

  await _admit_takeoff(tree, manager)

  async def wait_done(_client, body):
    # Both iterations terminal and the one final report delivered, while the
    # controller's portal loop is still alive.
    await _wait_for_iterations_settled(tree, body["child_session_id"], manager.id, timeout=30.0)

  body, _child_id = await _start_loop(cfg, session_blocks, tree, manager, repo, monkeypatch, wait_effect=wait_done)

  report = await _wait_for_final_report(tree, manager.id)
  assert report["outcome"] in ("completed", "blocked", "cancelled", "failed")
  state = await load_loop_state(manager.id, body["loop_id"], cfg)
  assert state is not None

  expected = claude_accounts.select(cfg, FABLE_MODEL)
  assert expected is not None and expected.label == "main"
  assert [b["kwargs"]["claude_account"] for b in builds] == [expected, expected]


@pytest.mark.asyncio
async def test_pool_exhausted_iteration_ends_the_loop_failed_with_a_quota_reason(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
  """select returns None: the iteration Run fails before any process starts and
  its events log carries the pool-exhausted error (with the earliest reset
  time); the loop classifies it as a quota blocker and ends at that iteration
  failed — no second iteration — and the parent is woken once."""
  cfg, session_blocks, tree, manager = await _pooled_pm_manager(tmp_path, monkeypatch)
  # An empty build queue: a spawned process would pop from it and fail loudly.
  builds = install_scripted_backends(monkeypatch, [], WORKER_BUILD_BACKEND_PATCH_TARGET)
  for label in ("main", "ext-1", "ext-2"):
    claude_accounts.observe_rate_limit(label, rate_limit_event("rejected", 1.0)["rate_limit_info"])
  assert claude_accounts.select(cfg, FABLE_MODEL) is None

  wakes = _count_manager_wakes(monkeypatch, tree, manager.id)
  await _admit_takeoff(tree, manager)
  body, child_id = await _start_loop(
      cfg,
      session_blocks,
      tree,
      manager,
      repo,
      monkeypatch,
      wait_effect=lambda _client, _body: _wait_for_final_report(tree, manager.id))

  report = await _wait_for_final_report(tree, manager.id)
  assert report["outcome"] == "failed"
  assert report["child_session_id"] == child_id

  records = tree.runs.list_run_records_sync(child_id)
  assert [r.kind for r in records] == ["iteration"]  # no further iteration launched
  run = records[0]
  assert tree.runs.terminal_outcome(tree.runs.load_events_sync(child_id), run.id) == "failed"
  assert run.pid is None
  assert builds == []  # no process ever started

  events_path = tree.runs.run_dir(child_id, run.id) / RUN_EVENTS_NAME
  error_lines = [json.loads(line) for line in events_path.read_text(encoding="utf-8").splitlines() if line.strip()]
  errors = [e for e in error_lines if e.get("type") == ET.ERROR]
  assert len(errors) == 1
  assert claude_relay.POOL_EXHAUSTED_PHRASE in errors[0]["message"]
  assert "earliest reset" in errors[0]["message"] and "UTC" in errors[0]["message"]

  state = await load_loop_state(manager.id, body["loop_id"], cfg)
  assert state is not None and state.status == "failed"
  # The quota failure closed the child cancelled with the loop's failed
  # outcome; the one final report carries the blocked-on-iteration sentence
  # the durable events log proves.
  assert tree.task_state(child_id) == "cancelled"
  _assert_single_close(tree, child_id, lifecycle="cancelled", sequence_outcome="failed")
  assert len(_final_reports(tree, manager.id)) == 1
  assert report["summary"].startswith(
      "[Improve loop 1] Improve loop blocked on iteration 1: "
      "launch refused with quota exhausted:")
  failed_payloads = [e for e in tree.events.load_events(manager.id) if e.get("type") == improve_sequence.IMPROVE_FAILED]
  assert len(failed_payloads) == 1
  assert failed_payloads[0]["blocked_iteration"] == 1
  # The error event's quota_exhausted flag makes the blocker; its reason carries the message.
  assert failed_payloads[0]["reason"] == f"launch refused with quota exhausted: {errors[0]['message']}"

  # The one wake: the final report's, through the delivery entry. The
  # quota-terminated iteration never reaches the delivery point, so no
  # per-iteration report exists.
  assert wakes == [manager.id]
  assert _iteration_reports(tree, manager.id) == []


@pytest.mark.parametrize(
    "payload_overrides, expected_outcome", [
        ({}, "blocked"),
        ({
            "merge_back": True
        }, "completed"),
    ],
    ids=["blocked", "completed"])
@pytest.mark.asyncio
async def test_loop_end_wakes_its_parent_exactly_once_and_a_replay_never_wakes(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path, payload_overrides: dict,
    expected_outcome: str) -> None:
  """The delivered final report is the parent's new durable input: the delivery
  entry wakes the parent exactly once per loop, whichever way it ends; a
  replayed delivery of the same report (created False) wakes nobody."""
  cfg, session_blocks, tree, manager = await _pooled_pm_manager(tmp_path, monkeypatch)
  _worker_backends(monkeypatch, ["iter one words", "iter two words"])
  wakes = _count_manager_wakes(monkeypatch, tree, manager.id)

  await _admit_takeoff(tree, manager)
  _body, child_id = await _start_loop(
      cfg,
      session_blocks,
      tree,
      manager,
      repo,
      monkeypatch,
      payload_overrides=payload_overrides,
      wait_effect=lambda _client, _body: _wait_for_final_report(tree, manager.id))

  report = await _wait_for_final_report(tree, manager.id)
  assert report["outcome"] == expected_outcome
  # Two per-iteration reports plus the final one: every freshly written
  # report woke the parent exactly once.
  assert len(_iteration_reports(tree, manager.id)) == 2
  assert wakes == [manager.id, manager.id, manager.id]
  # The loop's end closed the child on the plan's table: a landed merge-back
  # with successful iterations completes; an exhausted loop cancels.
  if expected_outcome == "completed":
    assert tree.task_state(child_id) == "completed"
    close = _assert_single_close(tree, child_id, lifecycle="completed", sequence_outcome="completed")
    records = tree.runs.list_run_records_sync(child_id)
    assert sorted(close["run_ids"]) == sorted(r.id for r in records)
  else:
    assert tree.task_state(child_id) == "cancelled"
    _assert_single_close(tree, child_id, lifecycle="cancelled", sequence_outcome="blocked")
  assert len(_final_reports(tree, manager.id)) == 1

  # The final report rides the child's close fact: a replayed delivery off
  # that same source re-derives the same stable report id, appends nothing,
  # and wakes nobody.
  closes = [e for e in tree.events.load_events(child_id) if e["type"] == ET.TASK_CLOSED]
  assert len(closes) == 1
  replayed, created = await tree.dispatch.deliver_child_report(
      child_id,
      source_event=closes[0],
      outcome=report["outcome"],
      summary=str(report["summary"]),
      result_refs=list(report["result_refs"]),
      recipient=manager.id)
  assert created is False and replayed["id"] == report["id"]
  assert wakes == [manager.id, manager.id, manager.id]


async def _await_wakes(wakes: list[str], manager_id: str, count: int, timeout: float = 10.0) -> None:
  """Wait until *count* parent wakes have fired (the final report's wake is
  awaited inline right after its append, so the poll only covers the tick)."""
  await _async_wait_for(lambda: len(wakes) >= count, timeout, "the parent wakes never reached the expected count")
  assert wakes == [manager_id] * count


@pytest.mark.asyncio
async def test_three_iterations_deliver_three_reports_and_wake_the_parent_four_times(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
  """Every judged iteration delivers ONE child report to the parent, and every
  freshly written report (3 per-iteration + 1 final) wakes it exactly once.
  The last iteration's report and the final report coexist under different
  ids: the per-iteration source id carries the improve-iteration prefix over
  the iteration's own run_finished event, so the final report (the raw latest
  run_finished) is never deduplicated away."""
  claude_accounts.reset_for_tests()
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  manager = await create_task(
      tree, parent=None, request_id="root", profile="manager", task=TaskSpec(goal="pm"), name="PM")
  builds = _worker_backends(monkeypatch, ["iter one words", "iter two words", "iter three words"])
  wakes = _count_manager_wakes(monkeypatch, tree, manager.id)

  await _admit_takeoff(tree, manager)
  _body, child_id = await _start_loop(
      cfg,
      session_blocks,
      tree,
      manager,
      repo,
      monkeypatch,
      payload_overrides={
          "iterations": 3,
          "work_branch": "improve/three"
      },
      wait_effect=lambda _client, _body: _wait_for_final_report(tree, manager.id))

  final = await _wait_for_final_report(tree, manager.id)
  await _await_wakes(wakes, manager.id, 4)

  iteration_reports = _iteration_reports(tree, manager.id)
  assert len(builds) == 3
  assert len(iteration_reports) == 3 and len(_final_reports(tree, manager.id)) == 1
  assert [e["outcome"] for e in iteration_reports] == ["success", "success", "success"]
  assert final["outcome"] == "blocked"  # exhausted without a proven landing
  # One wake per freshly written report: iteration 1, 2, 3, then the final.
  assert wakes == [manager.id, manager.id, manager.id, manager.id]
  # The last iteration's report and the final report are both present, with
  # different stable ids (child_event_id is the source id the report
  # deduplicates on).
  last = iteration_reports[-1]
  assert last["child_event_id"].startswith("improve-iteration:")
  assert not str(final["child_event_id"]).startswith("improve-iteration:")
  assert last["id"] != final["id"] and last["child_event_id"] != final["child_event_id"]
  # Each iteration report rides its own run_finished event and carries the
  # loop + run refs; the final report keeps its loop-only ref.
  records = tree.runs.list_run_records_sync(child_id)
  assert [r.kind for r in records] == ["iteration", "iteration", "iteration"]
  assert [e["result_refs"] for e in iteration_reports] == [["loop:1", f"run:{r.id}"] for r in records]
  assert final["result_refs"] == ["loop:1"]


@pytest.mark.asyncio
async def test_iteration_report_header_carries_the_judgment(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
  """The header names the iteration, report_valid, tip, commits_added and the
  report path. An iteration whose worker wrote no report file is invalid with
  the reason "no report file" (decided before the controller's fallback file
  is written); an iteration with a well-formed report is valid."""
  claude_accounts.reset_for_tests()
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  manager = await create_task(
      tree, parent=None, request_id="root", profile="manager", task=TaskSpec(goal="pm"), name="PM")
  loop_dir = cfg.sessions_dir / manager.id / "loops" / "1"

  async def write_iter_two_report() -> None:
    # The second worker writes a well-formed zero-progress report after its
    # stream: the mechanical verdict reads it as valid.
    (loop_dir / "iter_0002.md").write_text(
        "## Iter 2\n\nMeasured the goal reading; nothing to ship.\n\n### Commits\n\n"
        "- none \u2014 zero progress is acceptable per the goal.\n")

  backends = [
      SpawningScriptedBackend([result_event("iter one words")]),
      SpawningScriptedBackend([result_event("iter two words")], post_events=write_iter_two_report),
  ]
  monkeypatch.setattr(WORKER_BUILD_BACKEND_PATCH_TARGET, lambda *a, **k: backends.pop(0))
  stub_credentials({"charliebot": {"access_key": "op-secret"}})
  monkeypatch.setenv("CHARLIEBOT_HOME", str(cfg.charliebot_home))
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, make_pm_build("report noted"))

  await _admit_takeoff(tree, manager)
  _body, _child_id = await _start_loop(
      cfg,
      session_blocks,
      tree,
      manager,
      repo,
      monkeypatch,
      payload_overrides={"work_branch": "improve/header"},
      wait_effect=lambda _client, _body: _wait_for_final_report(tree, manager.id))

  iteration_reports = _iteration_reports(tree, manager.id)
  assert [e["outcome"] for e in iteration_reports] == ["success", "success"]
  first, second = iteration_reports
  # Iteration 1: no report file. The header says invalid with the reason, and
  # the body keeps the worker's own closing words (the fallback text).
  assert first["summary"].startswith("[Improve loop 1 · iteration 1/2] report_valid=false [invalid: no report file] ")
  assert f" report={loop_dir / 'iter_0001.md'} Audit per the improve-goal skill." in first["summary"]
  assert " tip=" in first["summary"] and " commits_added=0 " in first["summary"]
  assert first["summary"].endswith("\n\niter one words")
  # The controller's fallback file is marked and never flipped the verdict.
  fallback = (loop_dir / "iter_0001.md").read_text()
  assert fallback.startswith("<!-- runner fallback: worker wrote no report -->\n")
  assert "iter one words" in fallback
  # Iteration 2: a well-formed report is valid; the header carries the same
  # fields and the body is the report head.
  assert second["summary"].startswith("[Improve loop 1 · iteration 2/2] report_valid=true tip=")
  assert "[invalid:" not in second["summary"]
  assert f" report={loop_dir / 'iter_0002.md'} Audit per the improve-goal skill." in second["summary"]
  assert " commits_added=0 " in second["summary"]
  assert "## Iter 2" in second["summary"]


@pytest.mark.asyncio
async def test_failed_iteration_still_delivers_its_report_and_continues(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
  """A non-quota failed iteration is judged and delivered like any other
  (outcome failed, the worker's own closing words as the body) and the loop
  continues: only a quota blocker or a withheld launch skips the delivery
  point."""
  claude_accounts.reset_for_tests()
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  manager = await create_task(
      tree, parent=None, request_id="root", profile="manager", task=TaskSpec(goal="pm"), name="PM")
  failed = result_event("the build broke")
  failed["is_error"] = True  # an error result lands the failed durable outcome
  backends = [
      SpawningScriptedBackend([failed], exit_code=1),
      SpawningScriptedBackend([result_event("recovered words")]),
  ]
  monkeypatch.setattr(WORKER_BUILD_BACKEND_PATCH_TARGET, lambda *a, **k: backends.pop(0))
  stub_credentials({"charliebot": {"access_key": "op-secret"}})
  monkeypatch.setenv("CHARLIEBOT_HOME", str(cfg.charliebot_home))
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, make_pm_build("report noted"))

  await _admit_takeoff(tree, manager)
  _body, child_id = await _start_loop(
      cfg,
      session_blocks,
      tree,
      manager,
      repo,
      monkeypatch,
      payload_overrides={"work_branch": "improve/failed-iter"},
      wait_effect=lambda _client, _body: _wait_for_final_report(tree, manager.id))

  final = await _wait_for_final_report(tree, manager.id)
  iteration_reports = _iteration_reports(tree, manager.id)
  assert [e["outcome"] for e in iteration_reports] == ["failed", "success"]
  assert iteration_reports[0]["summary"].startswith(
      "[Improve loop 1 · iteration 1/2] report_valid=false [invalid: no report file] ")
  assert iteration_reports[0]["summary"].endswith("\n\nthe build broke")
  # The loop ran past the failure: both iterations exist and the loop ended
  # its own way (exhausted without a proven landing, not the iteration's
  # failure).
  records = tree.runs.list_run_records_sync(child_id)
  assert [r.kind for r in records] == ["iteration", "iteration"]
  assert final["outcome"] == "blocked"


@pytest.mark.asyncio
async def test_replaying_an_iteration_report_creates_no_event_and_wakes_nobody(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
  """A per-iteration report re-delivered under the same source id dedups: no
  second event lands in the parent's log and nobody is woken."""
  claude_accounts.reset_for_tests()
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  manager = await create_task(
      tree, parent=None, request_id="root", profile="manager", task=TaskSpec(goal="pm"), name="PM")
  _worker_backends(monkeypatch, ["iter one words"])
  wakes = _count_manager_wakes(monkeypatch, tree, manager.id)

  await _admit_takeoff(tree, manager)
  _body, child_id = await _start_loop(
      cfg,
      session_blocks,
      tree,
      manager,
      repo,
      monkeypatch,
      payload_overrides={
          "iterations": 1,
          "work_branch": "improve/replay"
      },
      wait_effect=lambda _client, _body: _wait_for_final_report(tree, manager.id))

  await _wait_for_final_report(tree, manager.id)
  await _await_wakes(wakes, manager.id, 2)  # the iteration report's + the final report's
  iteration_reports = _iteration_reports(tree, manager.id)
  assert len(iteration_reports) == 1
  first = iteration_reports[0]

  replayed, created = await tree.dispatch.deliver_child_report(
      child_id,
      source_event={"id": first["child_event_id"]},
      outcome=first["outcome"],
      summary=str(first["summary"]),
      result_refs=list(first["result_refs"]),
      recipient=manager.id)
  assert created is False and replayed["id"] == first["id"]
  assert len(_iteration_reports(tree, manager.id)) == 1
  assert wakes == [manager.id, manager.id]


@pytest.mark.asyncio
async def test_restart_recovery_marks_improve_loop_through_registered_controller(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The registered improve controller marks a prior-process loop interrupted once."""
  import os

  from src.features.improve.improve_command import (
      ImproveState,
      _active_loop_path,
      load_loop_state,
      save_loop_state,
  )
  from src.features.improve.sequence_controller import ImproveSequenceController
  from src.runtime.hooks.sequence_controllers import sequence_controllers
  from src.runtime.task_recovery import reconcile_task_tree

  cfg, _session_blocks, tree = build_env(tmp_path, monkeypatch)
  monkeypatch.setenv("CHARLIEBOT_HOME", str(cfg.charliebot_home))
  manager = await create_task(
      tree, parent=None, request_id="root", profile="manager", task=TaskSpec(goal="pm"), name="PM")
  state = ImproveState(
      loop_id=1,
      goal="improve the thing",
      status="running",
      work_branch="improve/restart",
      base_branch="main",
      repo_path=str(tmp_path),
      created_at="2026-10-07T00:00:00+00:00",
      server_pid=os.getpid() + 1,
  )
  await save_loop_state(manager.id, state, cfg)
  active_lock = _active_loop_path(manager.id, cfg)
  active_lock.write_text("1\n")

  improve_controller = next(c for c in sequence_controllers() if isinstance(c, ImproveSequenceController))
  reconcile = improve_controller.reconcile_interrupted
  controller_calls: list[tuple[object, object]] = []

  async def record_controller_call(cfg_arg, tree_arg) -> None:
    controller_calls.append((cfg_arg, tree_arg))
    await reconcile(cfg_arg, tree_arg)

  monkeypatch.setattr(improve_controller, "reconcile_interrupted", record_controller_call)
  await reconcile_task_tree(cfg, tree)

  recovered = await load_loop_state(manager.id, state.loop_id, cfg)
  assert recovered is not None and recovered.status == "interrupted"
  assert not active_lock.exists()
  notices = [e for e in tree.events.load_events(manager.id) if e.get("type") == improve_sequence.IMPROVE_FAILED]
  assert len(notices) == 1
  assert notices[0]["goal"] == state.goal
  assert controller_calls == [(cfg, tree)]


@pytest.mark.asyncio
async def test_improve_controller_preserves_base_lookups_and_skips_recovery(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The kind lookup selects the improve controller; it binds, owns, and lists
    nothing, and recovery leaves an iteration Run alone (no replay, no follow-up)."""
  from datetime import UTC, datetime
  from unittest.mock import AsyncMock

  from src.features.improve.improve_command import ImproveState, save_loop_state
  from src.features.improve.improve_sequence import loop_owner_ref
  from src.features.improve.sequence_controller import ImproveSequenceController
  from src.infra.models import RunRecord, SequenceRef
  from src.runtime.hooks.sequence_controllers import (
      binding_for,
      controller_for_sequence,
      sequence_controllers,
      sequence_listing_fields,
  )
  from src.runtime.task_execution import _replay_followups

  cfg, _session_blocks, tree = build_env(tmp_path, monkeypatch)
  monkeypatch.setenv("CHARLIEBOT_HOME", str(cfg.charliebot_home))
  manager = await create_task(
      tree, parent=None, request_id="root", profile="manager", task=TaskSpec(goal="pm"), name="PM")
  state = ImproveState(
      loop_id=1,
      goal="improve the thing",
      status="running",
      work_branch="improve/lookups",
      base_branch="main",
      repo_path=str(tmp_path),
      created_at="2026-10-07T00:00:00+00:00",
      server_pid=0,
  )
  await save_loop_state(manager.id, state, cfg)

  controllers = sequence_controllers()
  improve_controller = next(c for c in controllers if isinstance(c, ImproveSequenceController))
  base_controllers = tuple(c for c in controllers if c is not improve_controller)
  owner_ref = loop_owner_ref(manager.id, state.loop_id, cfg)
  assert improve_controller.sequence_kind == "improve"
  ref = SequenceRef(kind="improve", owner_ref=owner_ref, position=1)
  assert controller_for_sequence(ref) is improve_controller

  base_binding = next((binding for c in base_controllers if (binding := c.binding(manager.id)) is not None), None)
  assert binding_for(manager.id) is base_binding
  base_ownership = any(c.owns_session(manager) for c in base_controllers)
  assert improve_controller.owns_session(manager) is False
  assert any(c.owns_session(manager) for c in controllers) is base_ownership

  now_utc = datetime.now(UTC)
  base_listing = {manager.id: {}}
  for controller in base_controllers:
    for session_id, fields in controller.listing_fields((manager.id,), now_utc).items():
      base_listing.setdefault(session_id, {}).update(fields)
  assert improve_controller.listing_fields((manager.id,), now_utc) == {}
  assert sequence_listing_fields((manager.id,), now_utc) == base_listing

  redrive_mocks = []
  for controller in controllers:
    mock = AsyncMock()
    monkeypatch.setattr(controller, "redrive", mock)
    redrive_mocks.append(mock)
  await tree.runs.register_run(RunRecord(id="it-run", session_id=manager.id, kind="iteration", sequence_ref=ref))
  counters = {"followups": 0}
  await _replay_followups(manager.id, tree, None, counters, cfg)
  assert counters["followups"] == 0
  assert all(mock.await_count == 0 for mock in redrive_mocks)


# ---------------------------------------------------------------------------
# The loop close step: every loop end closes its worker child, exactly once
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_merge_back_with_only_failed_iterations_closes_cancelled_with_completed_report(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
  """The landing succeeds but no iteration did: lifecycle cancelled (an
  empty-handed delivery is never completed), report outcome completed."""
  claude_accounts.reset_for_tests()
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  manager = await create_task(
      tree, parent=None, request_id="root", profile="manager", task=TaskSpec(goal="pm"), name="PM")
  failed = result_event("the build broke")
  failed["is_error"] = True
  backends = [
      SpawningScriptedBackend([failed], exit_code=1),
      SpawningScriptedBackend([dict(failed)], exit_code=1),
  ]
  monkeypatch.setattr(WORKER_BUILD_BACKEND_PATCH_TARGET, lambda *a, **k: backends.pop(0))
  stub_credentials({"charliebot": {"access_key": "op-secret"}})
  monkeypatch.setenv("CHARLIEBOT_HOME", str(cfg.charliebot_home))
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, make_pm_build("report noted"))

  await _admit_takeoff(tree, manager)
  body, child_id = await _start_loop(
      cfg,
      session_blocks,
      tree,
      manager,
      repo,
      monkeypatch,
      payload_overrides={
          "merge_back": True,
          "work_branch": "improve/failed-merge",
      },
      wait_effect=lambda _client, _body: _wait_for_final_report(tree, manager.id))

  state = await load_loop_state(manager.id, body["loop_id"], cfg)
  assert state is not None and state.status == "completed"
  assert tree.task_state(child_id) == "cancelled"
  close = _assert_single_close(tree, child_id, lifecycle="cancelled", sequence_outcome="completed")
  assert close["run_ids"] == []  # no successful iteration to cite
  finals = _final_reports(tree, manager.id)
  assert len(finals) == 1 and finals[0]["outcome"] == "completed"


@pytest.mark.asyncio
async def test_controller_exception_closes_the_child_failed(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
  """A controller failure after the iterations closes the child cancelled with
  the loop's failed outcome and exactly one final report."""
  claude_accounts.reset_for_tests()
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  manager = await create_task(
      tree, parent=None, request_id="root", profile="manager", task=TaskSpec(goal="pm"), name="PM")
  _worker_backends(monkeypatch, ["iter one words", "iter two words"])

  async def boom(*args, **kwargs):
    raise RuntimeError("injected landing failure")

  monkeypatch.setattr(improve_command, "_land_work_branch_after_loop", boom)

  await _admit_takeoff(tree, manager)
  body, child_id = await _start_loop(
      cfg,
      session_blocks,
      tree,
      manager,
      repo,
      monkeypatch,
      wait_effect=lambda _client, _body: _wait_for_final_report(tree, manager.id))

  state = await load_loop_state(manager.id, body["loop_id"], cfg)
  assert state is not None and state.status == "failed"
  assert tree.task_state(child_id) == "cancelled"
  _assert_single_close(tree, child_id, lifecycle="cancelled", sequence_outcome="failed")
  finals = _final_reports(tree, manager.id)
  assert len(finals) == 1 and finals[0]["outcome"] == "failed"
  assert finals[0]["summary"].startswith("[Improve loop 1] Improve loop failed.")
  assert "iter one words" in finals[0]["summary"] and "iter two words" in finals[0]["summary"]


@pytest.mark.asyncio
async def test_worktree_failure_closes_the_child_failed(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
  """A worktree creation failure before the first iteration closes the child
  cancelled with the loop's failed outcome and exactly one final report."""
  claude_accounts.reset_for_tests()
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  manager = await create_task(
      tree, parent=None, request_id="root", profile="manager", task=TaskSpec(goal="pm"), name="PM")
  builds = _worker_backends(monkeypatch, ["never launched"])

  async def boom(*args, **kwargs):
    raise RuntimeError("injected worktree failure")

  monkeypatch.setattr(improve_sequence.git, "git_create_worktree", boom)

  await _admit_takeoff(tree, manager)
  body, child_id = await _start_loop(
      cfg,
      session_blocks,
      tree,
      manager,
      repo,
      monkeypatch,
      wait_effect=lambda _client, _body: _wait_for_final_report(tree, manager.id))

  state = await load_loop_state(manager.id, body["loop_id"], cfg)
  assert state is not None and state.status == "failed"
  assert tree.task_state(child_id) == "cancelled"
  _assert_single_close(tree, child_id, lifecycle="cancelled", sequence_outcome="failed")
  finals = _final_reports(tree, manager.id)
  assert len(finals) == 1 and finals[0]["outcome"] == "failed"
  assert finals[0]["summary"] == "[Improve loop 1] Improve loop failed."
  assert builds == []  # no iteration ever launched
  assert tree.runs.list_run_records_sync(child_id) == []


@pytest.mark.asyncio
async def test_improve_stop_mid_iteration_closes_after_the_controller_ends(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
  """improve-stop during a running iteration: that iteration's finish runs
  after_run, which must not close the child (the controller still holds the
  active lock); the controller's own end path then closes it cancelled with
  the loop's cancelled outcome, and the final report holds the iteration's
  summary."""
  from src.features.improve.sequence_controller import ImproveSequenceController

  claude_accounts.reset_for_tests()
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  manager = await create_task(
      tree, parent=None, request_id="root", profile="manager", task=TaskSpec(goal="pm"), name="PM")
  gate = asyncio.Event()
  first = SpawningScriptedBackend([result_event("iter one words")], gate=gate.wait)
  second = SpawningScriptedBackend([result_event("never reached")])
  install_scripted_backends(monkeypatch, [first, second], WORKER_BUILD_BACKEND_PATCH_TARGET)
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, make_pm_build("report noted"))
  # Park the controller inside the judgment that follows the iteration's
  # finish, so the test observes the child between after_run and the
  # controller's own end path.
  release_judge = asyncio.Event()
  real_judge = improve_sequence._judge_iteration

  async def held_judge(*args):
    await release_judge.wait()
    return await real_judge(*args)

  monkeypatch.setattr(improve_sequence, "_judge_iteration", held_judge)
  after_run_calls: list[str] = []
  real_after_run = ImproveSequenceController.after_run

  async def recording_after_run(self, session_id, run, tree_, cfg_):
    after_run_calls.append(run.id)
    await real_after_run(self, session_id, run, tree_, cfg_)

  monkeypatch.setattr(ImproveSequenceController, "after_run", recording_after_run)

  body, child_id, controller = await _start_loop_in_process(cfg, tree, manager, repo, iterations=2)
  await _async_wait_for(lambda: bool(tree.runs.list_run_records_sync(child_id)), 10.0, "iteration never registered")
  assert await improve_command.stop_improve_loop(manager.id, cfg) is True
  gate.set()
  await _async_wait_for(lambda: bool(after_run_calls), 10.0, "after_run never ran")
  # The iteration's finish did not close the child: the controller is still
  # parked with its active lock held.
  assert tree.task_state(child_id) == "open"
  assert _close_facts(tree, child_id) == []

  release_judge.set()
  await asyncio.wait_for(controller, timeout=10)

  state = await load_loop_state(manager.id, body["loop_id"], cfg)
  assert state is not None and state.status == "stopped"
  assert tree.task_state(child_id) == "cancelled"
  _assert_single_close(tree, child_id, lifecycle="cancelled", sequence_outcome="cancelled")
  finals = _final_reports(tree, manager.id)
  assert len(finals) == 1 and finals[0]["outcome"] == "cancelled"
  assert finals[0]["summary"].startswith("[Improve loop 1] Improve loop stopped by user after 1 iteration(s);")
  assert "iter one words" in finals[0]["summary"]
  assert finals[0]["result_refs"] == ["loop:1"]
  # The stopped loop never registered a second iteration.
  assert len(tree.runs.list_run_records_sync(child_id)) == 1


@pytest.mark.asyncio
async def test_withheld_iteration_launch_closes_the_child_blocked(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
  """A withheld iteration launch: the queued Run takes the close step's stop
  request and never launches; the child closes cancelled with the loop's
  blocked outcome and its work state reads idle."""
  claude_accounts.reset_for_tests()
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  cfg.server.min_free_disk_gib = 10**9  # no filesystem holds this much: every worker launch is withheld
  manager = await create_task(
      tree, parent=None, request_id="root", profile="manager", task=TaskSpec(goal="pm"), name="PM")
  builds = _worker_backends(monkeypatch, ["never launched"])

  await _admit_takeoff(tree, manager)
  body, child_id = await _start_loop(
      cfg,
      session_blocks,
      tree,
      manager,
      repo,
      monkeypatch,
      wait_effect=lambda _client, _body: _wait_for_final_report(tree, manager.id))

  state = await load_loop_state(manager.id, body["loop_id"], cfg)
  assert state is not None and state.status == "blocked"
  assert tree.task_state(child_id) == "cancelled"
  _assert_single_close(tree, child_id, lifecycle="cancelled", sequence_outcome="blocked")
  records = tree.runs.list_run_records_sync(child_id)
  assert len(records) == 1 and records[0].pid is None
  events = tree.runs.load_events_sync(child_id)
  assert tree.runs.terminal_outcome(events, records[0].id) is None
  stops = [e for e in events if e["type"] == ET.RUN_STOP_REQUESTED and e.get("run_id") == records[0].id]
  assert len(stops) == 1 and stops[0]["request_id"] == "improve:1:close"
  assert tree.work_state_of(child_id) == "idle"
  assert builds == []  # no process ever started
  finals = _final_reports(tree, manager.id)
  assert len(finals) == 1 and finals[0]["outcome"] == "blocked"
  assert finals[0]["summary"].startswith("[Improve loop 1] Improve loop ran 0 iteration(s)")


@pytest.mark.asyncio
async def test_controller_exception_while_an_iteration_runs_closes_after_its_finish(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
  """The controller fails while its iteration still runs: the close step
  leaves the child open, and the iteration's own finish closes it cancelled
  with the loop's failed outcome."""
  claude_accounts.reset_for_tests()
  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  manager = await create_task(
      tree, parent=None, request_id="root", profile="manager", task=TaskSpec(goal="pm"), name="PM")
  hold = asyncio.Event()
  backend = SpawningScriptedBackend([result_event("iter words")], post_events=hold.wait)

  def one_backend(option, cfg_, **kwargs):
    on_spawn = kwargs.get("on_spawn")
    if on_spawn is not None:
      backend.set_on_spawn(on_spawn)
    return backend

  monkeypatch.setattr(WORKER_BUILD_BACKEND_PATCH_TARGET, one_backend)
  adapter = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  tree.dispatch.executor = adapter
  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, make_pm_build("report noted"))
  real_settle = adapter.launch_and_settle
  settle: dict = {}

  async def raising_settle(session_id, run_id, *, prompt=None):
    settle["task"] = asyncio.create_task(real_settle(session_id, run_id, prompt=prompt))
    await _async_wait_for(
        lambda: any(r.pid is not None for r in tree.runs.list_run_records_sync(session_id)), 10.0,
        "the iteration never launched")
    raise RuntimeError("injected controller failure")

  monkeypatch.setattr(adapter, "launch_and_settle", raising_settle)

  body, child_id, controller = await _start_loop_in_process(cfg, tree, manager, repo, iterations=1)
  state_path = cfg.sessions_dir / manager.id / "loops" / str(body["loop_id"]) / "state.json"
  await _async_wait_for(
      lambda: state_path.is_file() and json.loads(state_path.read_text())["status"] == "failed", 10.0,
      "the controller's failure never landed in the loop state")
  # The iteration still owes its terminal fact: the child stays open.
  records = tree.runs.list_run_records_sync(child_id)
  assert len(records) == 1 and records[0].pid is not None
  assert tree.runs.terminal_outcome(tree.runs.load_events_sync(child_id), records[0].id) is None
  assert tree.task_state(child_id) == "open"
  assert _close_facts(tree, child_id) == []

  hold.set()
  await asyncio.wait_for(settle["task"], timeout=10)
  await asyncio.wait_for(controller, timeout=10)
  await _async_wait_for(
      lambda: tree.task_state(child_id) == "cancelled", 10.0, "the iteration's finish never closed the child")

  assert tree.runs.terminal_outcome(tree.runs.load_events_sync(child_id), records[0].id) == "success"
  _assert_single_close(tree, child_id, lifecycle="cancelled", sequence_outcome="failed")
  finals = _final_reports(tree, manager.id)
  assert len(finals) == 1 and finals[0]["outcome"] == "failed"


# ---------------------------------------------------------------------------
# The startup pass closes what a crash left open
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_startup_recovery_closes_an_interrupted_loop_whose_process_died(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Restart interruption with a dead iteration process: the reconcile marks
  the loop interrupted, the Run's drain lands its terminal fact, and its
  after_run closes the child cancelled with the loop's failed outcome."""
  from src.runtime.task_recovery import reconcile_task_tree

  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  monkeypatch.setenv("CHARLIEBOT_HOME", str(cfg.charliebot_home))
  manager = await create_task(
      tree, parent=None, request_id="root", profile="manager", task=TaskSpec(goal="pm"), name="PM")
  child_id = await _ended_loop_shape(
      cfg, tree, manager, status="running", iterations=[(None, None)], server_pid=os.getpid() + 1)
  # The launched iteration died with the old server: a recorded pid that no
  # longer exists, no terminal fact, and the drain's raw-log truth staged.
  run = await tree.runs.get_run(child_id, "iter-run-1")
  assert run is not None
  run.pid = 999999
  run.pid_start = "1-424000"
  run.started_at = datetime(2026, 10, 8, tzinfo=UTC)
  await tree.runs.write_record(child_id, run)
  write_raw_result(tree.runs.run_dir(child_id, run.id), "crashed mid-iteration")
  adapter = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  tree.dispatch.executor = adapter
  install_worker_launch_and_resume_backends(monkeypatch, [])
  _count_manager_wakes(monkeypatch, tree, manager.id)

  await reconcile_task_tree(cfg, tree)

  state = await load_loop_state(manager.id, 1, cfg)
  assert state is not None and state.status == "interrupted"
  assert tree.task_state(child_id) == "cancelled"
  _assert_single_close(tree, child_id, lifecycle="cancelled", sequence_outcome="failed")
  finals = _final_reports(tree, manager.id)
  assert len(finals) == 1 and finals[0]["outcome"] == "failed"


@pytest.mark.asyncio
async def test_startup_recovery_leaves_a_live_iterations_child_open_until_its_finish(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Restart interruption with the iteration process still alive: startup
  follows the process without waiting and the child stays open; the process's
  later finish closes it cancelled with the loop's failed outcome."""
  import src.runtime.runs as runs_mod
  import src.runtime.task_execution as task_execution_module

  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  monkeypatch.setenv("CHARLIEBOT_HOME", str(cfg.charliebot_home))
  manager = await create_task(
      tree, parent=None, request_id="root", profile="manager", task=TaskSpec(goal="pm"), name="PM")
  child_id = await _ended_loop_shape(
      cfg, tree, manager, status="running", iterations=[(None, None)], server_pid=os.getpid() + 1)
  run = await tree.runs.get_run(child_id, "iter-run-1")
  assert run is not None
  run.pid = 424777
  run.pid_start = "1-424000"
  run.started_at = datetime(2026, 10, 8, tzinfo=UTC)
  await tree.runs.write_record(child_id, run)
  monkeypatch.setattr(runs_mod, "is_run_alive", lambda *args, **kwargs: True)
  adapter = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  tree.dispatch.executor = adapter
  follows: list[tuple[str, str]] = []
  monkeypatch.setattr(adapter, "follow_run_in_background", lambda s, r: follows.append((s, r)))
  _count_manager_wakes(monkeypatch, tree, manager.id)

  # The startup pass itself: the loop is marked interrupted and the live Run
  # is followed, and nothing waits for the process or closes the child.
  await improve_sequence.reconcile_interrupted_sequences(cfg, tree)
  counters = {"resumed": 0, "drained": 0, "followups": 0}
  await task_execution_module._reconcile_node(child_id, tree, adapter, counters, cfg)

  state = await load_loop_state(manager.id, 1, cfg)
  assert state is not None and state.status == "interrupted"
  assert counters["resumed"] == 1 and follows == [(child_id, run.id)]
  assert tree.task_state(child_id) == "open"
  assert _close_facts(tree, child_id) == []

  # The process ends: the follow lands the terminal fact and after_run
  # closes the child.
  await tree.dispatch.finish_run(child_id, run.id, outcome="success")
  meta = await tree.load_meta(child_id)
  assert meta is not None
  fresh = await tree.runs.get_run(child_id, run.id)
  assert fresh is not None
  await adapter._after_worker_run(meta, fresh, "success")

  assert tree.task_state(child_id) == "cancelled"
  _assert_single_close(tree, child_id, lifecycle="cancelled", sequence_outcome="failed")
  finals = _final_reports(tree, manager.id)
  assert len(finals) == 1 and finals[0]["outcome"] == "failed"


@pytest.mark.asyncio
async def test_startup_recovers_a_stop_requested_launched_iteration_and_closes(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A launched iteration carrying a durable stop request at startup:
  recovery lands its interrupted finish and recover_run closes the child."""
  from src.runtime.control_events import ACTOR_SYSTEM, build_control_event
  from src.runtime.task_recovery import reconcile_task_tree

  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  monkeypatch.setenv("CHARLIEBOT_HOME", str(cfg.charliebot_home))
  manager = await create_task(
      tree, parent=None, request_id="root", profile="manager", task=TaskSpec(goal="pm"), name="PM")
  child_id = await _ended_loop_shape(
      cfg, tree, manager, status="running", iterations=[(None, None)], server_pid=os.getpid() + 1)
  proc = subprocess.Popen(["/bin/sleep", "30"])
  try:
    pid, pid_start = identity_of(proc.pid)
    run = await tree.runs.get_run(child_id, "iter-run-1")
    assert run is not None
    run.pid = pid
    run.pid_start = pid_start
    run.started_at = datetime.now(UTC)
    await tree.runs.write_record(child_id, run)
    # The durable stop request, staged as the old process left it (no follow).
    await tree.events.append(
        child_id,
        build_control_event(
            ET.RUN_STOP_REQUESTED,
            actor=ACTOR_SYSTEM,
            source_session_id=child_id,
            request_id="boot-stop",
            run_id=run.id))
    adapter = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
    tree.dispatch.executor = adapter
    install_worker_launch_and_resume_backends(monkeypatch, [])
    _count_manager_wakes(monkeypatch, tree, manager.id)

    await reconcile_task_tree(cfg, tree)

    assert proc.poll() is not None  # the stop's signal landed
    assert tree.runs.terminal_outcome(tree.runs.load_events_sync(child_id), run.id) == "interrupted"
    assert tree.task_state(child_id) == "cancelled"
    _assert_single_close(tree, child_id, lifecycle="cancelled", sequence_outcome="failed")
    finals = _final_reports(tree, manager.id)
    assert len(finals) == 1 and finals[0]["outcome"] == "failed"
  finally:
    if proc.poll() is None:
      proc.kill()


@pytest.mark.asyncio
async def test_startup_closes_a_loop_that_crashed_between_final_state_and_close(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Crash window after the final state write and before the close: the
  startup pass closes the child per the table and the parent receives one
  final report carrying the judged iterations' summaries."""
  from src.runtime.task_recovery import reconcile_task_tree

  cfg, _session_blocks, tree = build_env(tmp_path, monkeypatch)
  manager = await create_task(
      tree, parent=None, request_id="root", profile="manager", task=TaskSpec(goal="pm"), name="PM")
  child_id = await _ended_loop_shape(
      cfg,
      tree,
      manager,
      status="blocked",
      iterations=[("success", "## Iter 1\n\niter one report\n"), ("success", "## Iter 2\n\niter two report\n")])
  _count_manager_wakes(monkeypatch, tree, manager.id)

  await reconcile_task_tree(cfg, tree)

  assert tree.task_state(child_id) == "cancelled"
  _assert_single_close(tree, child_id, lifecycle="cancelled", sequence_outcome="blocked")
  finals = _final_reports(tree, manager.id)
  assert len(finals) == 1 and finals[0]["outcome"] == "blocked"
  assert finals[0]["summary"].startswith("[Improve loop 1] Improve loop ran 2 iteration(s)")
  assert "iter one report" in finals[0]["summary"] and "iter two report" in finals[0]["summary"]
  assert finals[0]["result_refs"] == ["loop:1"]


@pytest.mark.asyncio
async def test_startup_closes_a_loop_that_crashed_with_an_iteration_registered_unlaunched(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Crash window with an iteration registered and never launched: the close
  step stop-requests the queued Run, the child closes cancelled with the
  loop's blocked outcome, and the queued Run never launches afterwards."""
  from src.runtime.task_recovery import reconcile_task_tree

  cfg, session_blocks, tree = build_env(tmp_path, monkeypatch)
  adapter = _adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  tree.dispatch.executor = adapter
  manager = await create_task(
      tree, parent=None, request_id="root", profile="manager", task=TaskSpec(goal="pm"), name="PM")
  child_id = await _ended_loop_shape(
      cfg, tree, manager, status="blocked", iterations=[("success", "## Iter 1\n\niter one report\n"), (None, None)])
  _count_manager_wakes(monkeypatch, tree, manager.id)

  await reconcile_task_tree(cfg, tree)

  assert tree.task_state(child_id) == "cancelled"
  _assert_single_close(tree, child_id, lifecycle="cancelled", sequence_outcome="blocked")
  events = tree.runs.load_events_sync(child_id)
  queued = await tree.runs.get_run(child_id, "iter-run-2")
  assert queued is not None and queued.pid is None
  assert tree.runs.terminal_outcome(events, "iter-run-2") is None
  assert tree.runs.stop_requested(events, "iter-run-2")
  assert tree.work_state_of(child_id) == "idle"
  finals = _final_reports(tree, manager.id)
  assert len(finals) == 1 and finals[0]["outcome"] == "blocked"


@pytest.mark.asyncio
async def test_startup_closes_a_loop_that_crashed_between_run_finished_and_after_run(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Crash window after run_finished and before after_run: the startup pass
  closes the child per the table and the parent receives one final report."""
  from src.runtime.task_recovery import reconcile_task_tree

  cfg, _session_blocks, tree = build_env(tmp_path, monkeypatch)
  manager = await create_task(
      tree, parent=None, request_id="root", profile="manager", task=TaskSpec(goal="pm"), name="PM")
  child_id = await _ended_loop_shape(
      cfg, tree, manager, status="stopped", iterations=[("success", "## Iter 1\n\niter one report\n")])
  _count_manager_wakes(monkeypatch, tree, manager.id)

  await reconcile_task_tree(cfg, tree)

  assert tree.task_state(child_id) == "cancelled"
  _assert_single_close(tree, child_id, lifecycle="cancelled", sequence_outcome="cancelled")
  finals = _final_reports(tree, manager.id)
  assert len(finals) == 1 and finals[0]["outcome"] == "cancelled"
  assert "iter one report" in finals[0]["summary"]


@pytest.mark.asyncio
async def test_raced_close_step_calls_land_one_close_and_one_report(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Two call points running the close step at the same time (after_run
  racing a controller end path): one close fact and one final report."""
  cfg, _session_blocks, tree = build_env(tmp_path, monkeypatch)
  manager = await create_task(
      tree, parent=None, request_id="root", profile="manager", task=TaskSpec(goal="pm"), name="PM")
  child_id = await _ended_loop_shape(
      cfg, tree, manager, status="blocked", iterations=[("success", "## Iter 1\n\niter one report\n")])
  _count_manager_wakes(monkeypatch, tree, manager.id)

  await asyncio.gather(
      improve_sequence.close_ended_loop_child(tree, cfg, manager.id, 1),
      improve_sequence.close_ended_loop_child(tree, cfg, manager.id, 1))

  assert tree.task_state(child_id) == "cancelled"
  _assert_single_close(tree, child_id, lifecycle="cancelled", sequence_outcome="blocked")
  finals = _final_reports(tree, manager.id)
  assert len(finals) == 1 and finals[0]["outcome"] == "blocked"


@pytest.mark.asyncio
async def test_a_reopened_child_stays_open_across_restarts(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The user reopens a closed loop child: the next startup's close step
  returns at the recorded close before any stop request — the child stays
  open and its queued Run gains no new stop request."""
  cfg, _session_blocks, tree = build_env(tmp_path, monkeypatch)
  manager = await create_task(
      tree, parent=None, request_id="root", profile="manager", task=TaskSpec(goal="pm"), name="PM")
  child_id = await _ended_loop_shape(
      cfg, tree, manager, status="blocked", iterations=[("success", "## Iter 1\n\niter one report\n"), (None, None)])
  _count_manager_wakes(monkeypatch, tree, manager.id)

  await improve_sequence.close_ended_loop_child(tree, cfg, manager.id, 1)
  assert tree.task_state(child_id) == "cancelled"
  events = tree.runs.load_events_sync(child_id)
  assert len([e for e in events if e["type"] == ET.RUN_STOP_REQUESTED]) == 1

  await tree.completion.restore_task(child_id, request_id="reopen-1", reason="operator retry", caller=OPERATOR)
  assert tree.task_state(child_id) == "open"

  await improve_sequence.reconcile_interrupted_sequences(cfg, tree)

  assert tree.task_state(child_id) == "open"
  events = tree.runs.load_events_sync(child_id)
  assert len([e for e in events if e["type"] == ET.RUN_STOP_REQUESTED]) == 1
  assert len(_close_facts(tree, child_id)) == 1
  assert len(_final_reports(tree, manager.id)) == 1
