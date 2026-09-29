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
from pathlib import Path

import pytest
from conftest import (
    FABLE_MODEL,
    POOLED_FABLE_ID,
    WORKER_BUILD_BACKEND_PATCH_TARGET,
    create_task,
    patch_instructions_content,
    rate_limit_event,
    stub_credentials,
)

from src.core import claude_accounts, claude_relay
from src.core import event_types as ET
from src.core.improve_command import load_loop_state
from src.core.models import SessionMetadata, TaskSpec
from src.core.runs import RUN_EVENTS_NAME
from tests.test_task_execution import (
    BUILD_BACKEND_PATCH_TARGET,
    OPERATOR,
    SpawningScriptedBackend,
    WorkerAccountRecorder,
    _adapter_with_silent_broadcast,
    build_env,
    build_pooled_env,
    init_repo_with_origin,
    install_backends,
    make_api_client,
    make_pm_build,
    result_event,
)


def _worker_backends(monkeypatch, outcomes: list[str]) -> list:
  """One scripted worker per iteration build, in launch order."""
  return install_backends(
      monkeypatch, [SpawningScriptedBackend([result_event(text)]) for text in outcomes],
      WORKER_BUILD_BACKEND_PATCH_TARGET)


async def _start_loop(cfg, session_mgr, tree, manager, monkeypatch, payload_overrides=None, wait_effect=None):
  """POST the improve loop against the v2 manager and wait for the controller's child."""
  patch_instructions_content(monkeypatch)
  stub_credentials({"charliebot": {"access_key": "op-secret"}})
  monkeypatch.setenv("CHARLIEBOT_HOME", str(cfg.charliebot_home))
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
  # The final report's wake dispatches the manager's report-consuming turn;
  # script it through the registry builder so no external process starts.
  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, make_pm_build("report noted"))
  payload = {
      "session_id": manager.id,
      "goal": "## Goal\n\nimprove the thing\n",
      "iterations": 2,
      "repo_path": str(tmp_repo),
      "base_branch": "main",
      "work_branch": "improve/test-branch",
  }
  payload.update(payload_overrides or {})
  with make_api_client(cfg, session_mgr, tree) as client:
    resp = client.post("/api/internal/improve", json=payload, headers=OPERATOR)
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


tmp_repo: Path = Path("/")  # replaced per-test by the fixture


@pytest.fixture()
def repo(tmp_path: Path) -> Path:
  global tmp_repo
  tmp_repo, _origin = init_repo_with_origin(tmp_path / "improve-repo")
  return tmp_repo


@pytest.mark.asyncio
async def test_two_iterations_stay_one_child_with_ordered_runs(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
  """Two iterations: ONE child, two ordered iteration Runs, one final report."""
  cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
  manager = await create_task(
      tree, parent=None, request_id="root", profile="manager", task=TaskSpec(goal="pm"), name="PM")
  builds = _worker_backends(monkeypatch, ["iter one words", "iter two words"])

  await tree.dispatch.admit_input(
      manager.id, event_type=ET.USER, content="Take off. Run the improve loop.", actor="user")

  async def _wait_done(client, body):
    child_id = body["child_session_id"]
    deadline = asyncio.get_event_loop().time() + 30
    while asyncio.get_event_loop().time() < deadline:
      records = tree.runs.list_run_records_sync(child_id)
      reports = [e for e in tree.events.load_events(manager.id) if e.get("type") == ET.CHILD_REPORT]
      if len(records) == 2 and reports and all(
          tree.runs.terminal_outcome(tree.runs.load_events_sync(child_id), r.id) is not None for r in records):
        return
      await asyncio.sleep(0.1)
    pytest.fail(
        f"the sequence never finished: runs={[(r.id, r.kind) for r in tree.runs.list_run_records_sync(child_id)]} "
        f"reports={[e for e in tree.events.load_events(manager.id) if e.get('type') == ET.CHILD_REPORT]}")

  body, child_id = await _start_loop(cfg, session_mgr, tree, manager, monkeypatch, wait_effect=_wait_done)

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
  # One final result report on the manager, from the child, after BOTH runs.
  deadline = asyncio.get_event_loop().time() + 10
  report = None
  while asyncio.get_event_loop().time() < deadline:
    events = tree.events.load_events(manager.id)
    reports = [e for e in events if e.get("type") == ET.CHILD_REPORT]
    if reports:
      report = reports[0]
      break
    await asyncio.sleep(0.1)
  assert report is not None, "the final sequence result was never delivered"
  assert report.get("child_session_id") == child_id
  assert report.get("outcome") in ("completed", "blocked", "failed", "cancelled")
  assert "Improve loop" in str(report.get("summary"))
  # The loop state is honestly 'blocked' — iterations exhausted without a
  # proven landing (no merge-back) is NOT successful delivery.
  state = await load_loop_state(manager.id, body["loop_id"], cfg)
  assert state is not None and state.status == "blocked"


@pytest.mark.asyncio
async def test_live_goal_change_affects_next_iteration(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
  """The controller re-reads goal.md every iteration: a mid-loop edit steers the next one."""
  cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
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
  patch_instructions_content(monkeypatch)
  stub_credentials({"charliebot": {"access_key": "op-secret"}})
  monkeypatch.setenv("CHARLIEBOT_HOME", str(cfg.charliebot_home))
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
  # The final report's wake dispatches the manager's report-consuming turn;
  # script it through the registry builder so no external process starts.
  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, make_pm_build("report noted"))

  await tree.dispatch.admit_input(
      manager.id, event_type=ET.USER, content="Take off. Run the improve loop.", actor="user")
  with make_api_client(cfg, session_mgr, tree) as client:
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
        headers=OPERATOR)
    assert resp.status_code == 200, resp.text
    child_id = resp.json()["child_session_id"]

    deadline = asyncio.get_event_loop().time() + 30
    while asyncio.get_event_loop().time() < deadline:
      records = tree.runs.list_run_records_sync(child_id)
      reports = [e for e in tree.events.load_events(manager.id) if e.get("type") == ET.CHILD_REPORT]
      if len(records) == 2 and reports and all(
          tree.runs.terminal_outcome(tree.runs.load_events_sync(child_id), r.id) is not None for r in records):
        break
      await asyncio.sleep(0.1)
    else:
      pytest.fail("both iterations never finished")
  # The second build's prompt carries the EDITED goal, not the original one.
  assert second.prompt is not None
  assert "now improve the OTHER thing" in second.prompt
  assert "improve the thing" not in second.prompt.split("Previous iteration summaries")[0]


@pytest.mark.asyncio
async def test_improve_without_authorization_is_forbidden_not_a_server_error(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
  """No take-off anywhere in the chain: 403 with the gate's reason, never a
    500, and nothing reserved."""
  from src.core.improve_command import _active_loop_path, _loops_dir, find_running_loop
  cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
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
  with make_api_client(cfg, session_mgr, tree) as client:
    resp = client.post("/api/internal/improve", json=payload, headers=OPERATOR)
  assert resp.status_code == 403, resp.text
  assert "take off" in str(resp.json()["detail"]).lower() or "authorization" in str(resp.json()["detail"]).lower()
  assert not _active_loop_path(manager.id, cfg).exists()
  assert await find_running_loop(manager.id, cfg) is None
  loops_dir = _loops_dir(manager.id, cfg)
  assert not loops_dir.is_dir() or list(loops_dir.glob("*")) == []


# ---------------------------------------------------------------------------
# Withheld launches settle: no hung waiter, released lock, honest report
# ---------------------------------------------------------------------------

# ---------------------------------------------------------------------------
# Pooled launches: iteration Runs start on a Claude pool account
# ---------------------------------------------------------------------------


async def _wait_for_final_report(tree, manager_id: str, timeout: float = 30.0) -> dict:
  """The ONE final sequence result on the manager, from the child."""
  deadline = asyncio.get_event_loop().time() + timeout
  reports: list[dict] = []
  while asyncio.get_event_loop().time() < deadline:
    reports = [e for e in tree.events.load_events(manager_id) if e.get("type") == ET.CHILD_REPORT]
    if reports:
      return reports[0]
    await asyncio.sleep(0.1)
  pytest.fail(f"the final sequence result was never delivered: {reports}")


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


@pytest.mark.asyncio
async def test_pooled_iteration_launches_on_the_selected_pool_account(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
  """The improve controller's iteration Runs are fresh worker launches: with a
  non-empty pool and a cc-claude backend, each one hands Worker the account
  claude_accounts.select returned, and the relay loop is armed (the backend
  build receives the same account)."""
  claude_accounts.reset_for_tests()
  cfg, session_mgr, tree = build_pooled_env(tmp_path, monkeypatch)
  manager = await tree.create_task(
      request_id="root",
      task_parent_id=None,
      profile="manager",
      task=TaskSpec(goal="pm"),
      name="PM",
      backend=None,
      caller=OPERATOR)
  recorder = WorkerAccountRecorder()
  recorder.install(monkeypatch)
  builds = _worker_backends(monkeypatch, ["iter one words", "iter two words"])

  await tree.dispatch.admit_input(
      manager.id, event_type=ET.USER, content="Take off. Run the improve loop.", actor="user")

  async def wait_done(_client, body):
    # Both iterations terminal and the one final report delivered, while the
    # controller's portal loop is still alive.
    deadline = asyncio.get_event_loop().time() + 30
    while asyncio.get_event_loop().time() < deadline:
      records = tree.runs.list_run_records_sync(body["child_session_id"])
      reports = [e for e in tree.events.load_events(manager.id) if e.get("type") == ET.CHILD_REPORT]
      if len(records) == 2 and reports and all(
          tree.runs.terminal_outcome(tree.runs.load_events_sync(body["child_session_id"]), r.id) is not None
          for r in records):
        return
      await asyncio.sleep(0.1)
    pytest.fail("both iterations never finished")

  body, _child_id = await _start_loop(cfg, session_mgr, tree, manager, monkeypatch, wait_effect=wait_done)

  report = await _wait_for_final_report(tree, manager.id)
  assert report["outcome"] in ("completed", "blocked", "cancelled", "failed")
  state = await load_loop_state(manager.id, body["loop_id"], cfg)
  assert state is not None

  expected = claude_accounts.select(cfg, FABLE_MODEL)
  assert expected is not None and expected.label == "main"
  assert recorder.accounts == [expected, expected]
  assert [b["kwargs"]["claude_account"] for b in builds] == [expected, expected]


@pytest.mark.asyncio
async def test_pool_exhausted_iteration_ends_the_loop_failed_with_a_quota_reason(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, repo: Path) -> None:
  """select returns None: the iteration Run fails before any process starts and
  its events log carries the pool-exhausted error (with the earliest reset
  time); the loop classifies it as a quota blocker and ends at that iteration
  failed — no second iteration — and the parent is woken once."""
  claude_accounts.reset_for_tests()
  cfg, session_mgr, tree = build_pooled_env(tmp_path, monkeypatch)
  manager = await tree.create_task(
      request_id="root",
      task_parent_id=None,
      profile="manager",
      task=TaskSpec(goal="pm"),
      name="PM",
      backend=None,
      caller=OPERATOR)
  # An empty build queue: a spawned process would pop from it and fail loudly.
  builds = install_backends(monkeypatch, [], WORKER_BUILD_BACKEND_PATCH_TARGET)
  for label in ("main", "ext-1", "ext-2"):
    claude_accounts.observe_rate_limit(label, rate_limit_event("rejected", 1.0)["rate_limit_info"])
  assert claude_accounts.select(cfg, FABLE_MODEL) is None

  wakes = _count_manager_wakes(monkeypatch, tree, manager.id)
  await tree.dispatch.admit_input(
      manager.id, event_type=ET.USER, content="Take off. Run the improve loop.", actor="user")
  body, child_id = await _start_loop(
      cfg,
      session_mgr,
      tree,
      manager,
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
  failed_payloads = [e for e in tree.events.load_events(manager.id) if e.get("type") == ET.IMPROVE_FAILED]
  assert len(failed_payloads) == 1
  assert failed_payloads[0]["blocked_iteration"] == 1
  # The quota pattern matches the phrase's lowercase form (the classification
  # lowercases the event text before scanning).
  assert claude_relay.POOL_EXHAUSTED_PHRASE.lower() in failed_payloads[0]["reason"]

  # The one wake: the final report's, through the delivery entry.
  assert wakes == [manager.id]


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
  claude_accounts.reset_for_tests()
  cfg, session_mgr, tree = build_pooled_env(tmp_path, monkeypatch)
  manager = await tree.create_task(
      request_id="root",
      task_parent_id=None,
      profile="manager",
      task=TaskSpec(goal="pm"),
      name="PM",
      backend=None,
      caller=OPERATOR)
  _worker_backends(monkeypatch, ["iter one words", "iter two words"])
  wakes = _count_manager_wakes(monkeypatch, tree, manager.id)

  await tree.dispatch.admit_input(
      manager.id, event_type=ET.USER, content="Take off. Run the improve loop.", actor="user")
  _body, child_id = await _start_loop(
      cfg,
      session_mgr,
      tree,
      manager,
      monkeypatch,
      payload_overrides=payload_overrides,
      wait_effect=lambda _client, _body: _wait_for_final_report(tree, manager.id))

  report = await _wait_for_final_report(tree, manager.id)
  assert report["outcome"] == expected_outcome
  assert wakes == [manager.id]

  # The replay re-derives the same stable report id, appends nothing, and
  # wakes nobody.
  source = tree.dispatch.report_source_event(child_id, "improve child")
  replayed, created = await tree.dispatch.deliver_child_report(
      child_id,
      source_event=source,
      outcome=report["outcome"],
      summary=str(report["summary"]),
      result_refs=list(report["result_refs"]),
      recipient=manager.id)
  assert created is False and replayed["id"] == report["id"]
  assert wakes == [manager.id]
