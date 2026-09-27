"""Authorization-gate redrive: a withheld Run launches when its window opens.

A Run whose only launch blocker was the takeoff gate stays queued with no
terminal fact. The user's authorizing message used to leave it there until an
unrelated later dispatch happened to re-judge the node; the real message route
now re-drives the subtree the message authorizes through the normal launch
path, so the same ``execute_run`` prechecks every fresh launch passes decide
again. A message without the token, and a sibling subtree under a deeper
real-user node, keep their Runs queued.
"""

from __future__ import annotations

import asyncio
from pathlib import Path

import pytest
from conftest import (
    BUILD_BACKEND_PATCH_TARGET,
    WORKER_BUILD_BACKEND_PATCH_TARGET,
    patch_instructions_content,
    stub_credentials,
)

from src.core import event_types as ET
from src.core.models import TaskSpec
from src.core.run_token import CallerIdentity
from src.core.task_sessions import TaskTreeManager
from tests.test_task_execution import (
    SpawningScriptedBackend,
    _adapter_with_silent_broadcast,
    build_env,
    install_backends,
    result_event,
    wait_for_terminal_run,
)

OPERATOR = CallerIdentity(kind="operator")


def make_chat_client(cfg, session_mgr, tree):
  """The real message route over the synthetic instance (the established
  session-dispatch client shape: the chat router under /api/sessions)."""
  from fastapi import FastAPI
  from fastapi.testclient import TestClient

  import src.api.chat as chat_api
  from src.api.deps import get_config, get_run_store, get_session_manager, get_task_manager

  app = FastAPI()
  app.include_router(chat_api.router, prefix="/api/sessions")
  stub_credentials({"charliebot": {"access_key": "op-secret"}})
  app.dependency_overrides[get_config] = lambda: cfg
  app.dependency_overrides[get_session_manager] = lambda: session_mgr
  app.dependency_overrides[get_task_manager] = lambda: tree
  app.dependency_overrides[get_run_store] = lambda: tree.runs
  return TestClient(app)


async def build_manager_with_worker(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> tuple[object, object, TaskTreeManager, object, object]:
  """One manager (no user authorization yet) and one implement worker under it."""
  cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
  patch_instructions_content(monkeypatch)
  manager = await tree.create_task(
      request_id="root",
      task_parent_id=None,
      profile="manager",
      task=TaskSpec(goal="pm"),
      name="PM",
      backend=None,
      caller=OPERATOR)
  worker = await tree.create_task(
      request_id="child",
      task_parent_id=manager.id,
      profile="worker",
      task=TaskSpec(goal="do the work"),
      name="W",
      backend=None,
      caller=OPERATOR)
  # The WORKER path's deferred loader binds src.agents.worker.build_backend on
  # first access from whatever src.agents.backends.registry.build_backend holds
  # at that moment, so the worker target must be patched BEFORE the registry
  # target: a registry stand-in active at the first worker access would be
  # bound as the worker "previous" value and restored permanently at undo.
  install_backends(
      monkeypatch, [SpawningScriptedBackend([result_event("authorized work")])], WORKER_BUILD_BACKEND_PATCH_TARGET)
  # Manager turns ride the build-registry path.
  install_backends(
      monkeypatch,
      [SpawningScriptedBackend([result_event("taken off")]),
       SpawningScriptedBackend([result_event("later")])], BUILD_BACKEND_PATCH_TARGET)
  return cfg, session_mgr, tree, manager, worker


async def dispatch_withheld_work_run(tree: TaskTreeManager, worker) -> str:
  """Dispatch one work Run on *worker* and settle its authorization-withheld
  launch: the manager holds no real user authorization, so the Run stays
  queued with its batch claimed and no process."""
  await tree.dispatch.admit_input(worker.id, event_type=ET.USER, content="Start the task.", actor="user")
  decision = await tree.dispatch.dispatch_pending(worker.id)
  assert decision["launch"] is True, decision
  run_id = str(decision["run_id"])
  observation = await tree.dispatch.executor.launch_and_settle(worker.id, run_id)
  assert observation.withheld is not None, observation
  assert "Delegation blocked" in observation.withheld
  run = await tree.runs.get_run(worker.id, run_id)
  assert run is not None and run.pid is None, "a withheld launch must never start a process"
  events = tree.runs.load_events_sync(worker.id)
  assert not tree.runs.run_has_terminal_fact(run, events)
  return run_id


async def assert_stays_queued(tree: TaskTreeManager, session_id: str, run_id: str) -> None:
  """Bounded watch: the Run never leaves queued (no process, no terminal fact)."""
  deadline = asyncio.get_event_loop().time() + 2.0
  while asyncio.get_event_loop().time() < deadline:
    run = await tree.runs.get_run(session_id, run_id)
    assert run is not None
    events = tree.runs.load_events_sync(session_id)
    assert run.pid is None and not tree.runs.run_has_terminal_fact(run, events), (
        f"run {run_id} left queued without its authorization: pid={run.pid}")
    await asyncio.sleep(0.05)


@pytest.mark.asyncio
async def test_take_off_message_launches_the_authorization_withheld_run(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The user's take off on the authorizing ancestor re-drives the queued Run
  through the normal launch path: the same execute_run prechecks pass and the
  Run runs to success."""
  cfg, session_mgr, tree, manager, worker = await build_manager_with_worker(tmp_path, monkeypatch)
  run_id = await dispatch_withheld_work_run(tree, worker)

  with make_chat_client(cfg, session_mgr, tree) as client:
    sent = client.post(f"/api/sessions/{manager.id}/message", json={"content": "Take off."})
    assert sent.status_code == 202, sent.text
    # The re-driven launch is scheduled by the request: it rides this client's
    # running loop, so the terminal wait stays inside the client context.
    _run, outcome = await wait_for_terminal_run(tree, worker.id, run_id)
  assert outcome == "success"


@pytest.mark.asyncio
async def test_message_without_the_token_keeps_the_run_queued(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A real user message that opens no window re-drives nothing: the Run stays
  queued, no process starts, no backend is built."""
  cfg, session_mgr, tree, manager, worker = await build_manager_with_worker(tmp_path, monkeypatch)
  builds = install_backends(
      monkeypatch, [SpawningScriptedBackend([result_event("never used")])], WORKER_BUILD_BACKEND_PATCH_TARGET)
  run_id = await dispatch_withheld_work_run(tree, worker)

  with make_chat_client(cfg, session_mgr, tree) as client:
    sent = client.post(f"/api/sessions/{manager.id}/message", json={"content": "hold on, new plan"})
    assert sent.status_code == 202, sent.text

  await assert_stays_queued(tree, worker.id, run_id)
  assert builds == [], "a message without the token must never build a backend"


@pytest.mark.asyncio
async def test_sibling_subtree_under_its_own_user_node_keeps_its_gate(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Scope is exactly the subtree the message's node authorizes: a deeper
  manager holding its own real user message keeps its own gate, so its
  worker's queued Run stays queued while the root's own worker launches."""
  cfg, session_mgr, tree, manager, worker = await build_manager_with_worker(tmp_path, monkeypatch)
  # A sub-manager with its own (non-authorizing) real user instruction.
  sub = await tree.create_task(
      request_id="sub",
      task_parent_id=manager.id,
      profile="manager",
      task=TaskSpec(goal="sub program"),
      name="Sub",
      backend=None,
      caller=OPERATOR)
  await tree.dispatch.admit_input(sub.id, event_type=ET.USER, content="carry on locally", actor="user")
  sub_decision = await tree.dispatch.dispatch_pending(sub.id)
  # The sub's own turn consumes the serialized slot before the withheld work
  # dispatches, so the takeoff request later meets an uncontended tree.
  await wait_for_terminal_run(tree, sub.id, str(sub_decision["run_id"]))
  sub_worker = await tree.create_task(
      request_id="sub-worker",
      task_parent_id=sub.id,
      profile="worker",
      task=TaskSpec(goal="sub work"),
      name="SW",
      backend=None,
      caller=OPERATOR)
  sub_run_id = await dispatch_withheld_work_run(tree, sub_worker)
  root_run_id = await dispatch_withheld_work_run(tree, worker)

  with make_chat_client(cfg, session_mgr, tree) as client:
    sent = client.post(f"/api/sessions/{manager.id}/message", json={"content": "Take off."})
    assert sent.status_code == 202, sent.text
    _run, outcome = await wait_for_terminal_run(tree, worker.id, root_run_id)
    await assert_stays_queued(tree, sub_worker.id, sub_run_id)
  assert outcome == "success"

  _run, outcome = await wait_for_terminal_run(tree, worker.id, root_run_id)
  assert outcome == "success"
  await assert_stays_queued(tree, sub_worker.id, sub_run_id)
