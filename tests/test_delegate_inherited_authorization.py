"""The runtime gate on the real v2 entry points.

The v2 policy is one central owner: the runtime gate
(``TaskTreeManager.check_task_authorization``), applied at the request
entries — delegation, improve, and agent messages to worker nodes — and never
re-judged at a Run's launch. A manager delegates through the real
/api/internal/delegate route when its open task chain holds a real user
message, a question included; a closed ancestor refuses; verify stays exempt
(read-only) on the route. Every scenario here runs the actual HTTP route
against a synthetic task tree with a scripted backend.
"""

from __future__ import annotations

import pathlib

import conftest
import pytest

from src.infra import event_types as ET
from src.infra import models
from src.runtime import run_token
from tests import test_task_execution


async def make_tree(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch):
  """One synthetic instance: root manager + child manager under it, plus the
    API client and the scripted executor, with no user input anywhere yet."""
  cfg, session_blocks, tree, root = await test_task_execution.build_manager_root(tmp_path, monkeypatch)
  child = await tree.create_task(
      request_id="child",
      task_parent_id=root.id,
      profile="manager",
      task=models.TaskSpec(goal="feature"),
      name="Feature",
      backend=None,
      caller="operator")
  return cfg, session_blocks, tree, root, child


@pytest.mark.asyncio
@pytest.mark.integration  # polls the delegated run's terminal fact in real time on the API loop
async def test_authorized_ancestor_delegates_without_a_local_user_message(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, repo: pathlib.Path) -> None:
  """The real route from a child manager with NO local user message and an
    authorized ancestor creates one leaf and one launched Run; a replay returns
    the same child/Run and never spawns a second process."""
  cfg, session_blocks, tree, root, child = await make_tree(tmp_path, monkeypatch)
  builds = conftest.install_scripted_backends(
      monkeypatch, [test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("leaf done")])],
      conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)
  await tree.dispatch.admit_input(root.id, event_type=ET.USER, content="Take off. Ship the feature.", actor="user")

  with test_task_execution.make_api_client(cfg, session_blocks, tree) as client:
    payload = conftest.delegate_payload(child.id, repo)
    first = client.post("/api/internal/delegate", json=payload, headers=test_task_execution.OP_HEADERS)
    assert first.status_code == 200, first.text
    body = first.json()
    child_leaf, run_id = body["session_id"], body["run_id"]
    assert body["parent_session_id"] == child.id
    leaf_meta = await tree.load_meta(child_leaf)
    assert leaf_meta is not None and leaf_meta.profile == "worker"
    assert leaf_meta.task_parent_id == child.id
    runs = tree.runs.list_run_records_sync(child_leaf)
    assert [r.id for r in runs] == [run_id]

    # The replayed request returns the original product, never a sibling.
    replay = client.post("/api/internal/delegate", json=payload, headers=test_task_execution.OP_HEADERS)
    assert replay.status_code == 200, replay.text
    assert replay.json()["session_id"] == child_leaf
    assert replay.json()["run_id"] == run_id

    # The run executes on the API loop that scheduled it: the wait stays
    # inside the client context (the same rule the execution tests pin).
    await test_task_execution.wait_for_terminal_run(tree, child_leaf, run_id)

  # ONE leaf and ONE process for the operation and its replay.
  leaves = [
      m.id
      for m in (await tree._get_index()).metas.values()
      if (await tree.load_meta(m.id)) is not None and (await tree.load_meta(m.id)).task_parent_id == child.id
  ]
  assert leaves == [child_leaf]
  assert len(builds) == 1


@pytest.mark.asyncio
async def test_user_questions_admit_root_and_child_delegations(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, repo: pathlib.Path) -> None:
  """No take off anywhere: the root's question starts the line, so the root delegates an
    implement worker, and the child manager, whose own user message is a question too,
    delegates on the same route."""
  cfg, session_blocks, tree, root, child = await make_tree(tmp_path, monkeypatch)
  monkeypatch.setattr(tree.dispatch.executor, "_schedule_launch", lambda *args, **kwargs: None)
  await tree.dispatch.admit_input(root.id, event_type=ET.USER, content="Can you fix the flaky test?", actor="user")
  await tree.dispatch.admit_input(child.id, event_type=ET.USER, content="Which file holds it?", actor="user")

  with test_task_execution.make_api_client(cfg, session_blocks, tree) as client:
    for caller_id in (root.id, child.id):
      resp = client.post(
          "/api/internal/delegate",
          json=conftest.delegate_payload(caller_id, repo, task_type="implement"),
          headers=test_task_execution.OP_HEADERS)
      assert resp.status_code == 200, resp.text
      leaf = await tree.load_meta(resp.json()["session_id"])
      assert leaf is not None and leaf.profile == "worker" and leaf.task_parent_id == caller_id


@pytest.mark.asyncio
async def test_closed_ancestor_refuses_delegation(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, repo: pathlib.Path) -> None:
  """The child holds its own user message, but the archived root closes the line: the route
    refuses and names the closed task, and nothing is created."""
  cfg, session_blocks, tree, root, child = await make_tree(tmp_path, monkeypatch)
  builds = conftest.install_scripted_backends(monkeypatch, [], conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)
  await tree.dispatch.admit_input(child.id, event_type=ET.USER, content="Can you fix it?", actor="user")
  await tree.events.append(
      root.id,
      tree.completion.archived_close_event(
          root.id,
          "close-root",
          summary="archived by the user",
          report_to=None,
          archived_with=None,
          actor="user",
          result_refs=[],
          run_ids=[]))

  with test_task_execution.make_api_client(cfg, session_blocks, tree) as client:
    resp = client.post(
        "/api/internal/delegate",
        json=conftest.delegate_payload(child.id, repo),
        headers=test_task_execution.OP_HEADERS)
  assert resp.status_code == 403
  assert f"ancestor task {root.id} is archived" in resp.json()["detail"]
  assert builds == []
  assert [m for m in (await tree._get_index()).metas.values() if m.task_parent_id == child.id] == []


@pytest.mark.asyncio
async def test_worker_caller_cannot_delegate(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, repo: pathlib.Path) -> None:
  """Delegating FROM a worker node is refused: agents run only under a
    manager task."""
  cfg, session_blocks, tree, root, child = await make_tree(tmp_path, monkeypatch)
  builds = conftest.install_scripted_backends(monkeypatch, [], conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)
  worker = await tree.create_task(
      request_id="w",
      task_parent_id=child.id,
      profile="worker",
      task=models.TaskSpec(goal="leaf"),
      name="W",
      backend=None,
      caller="operator")
  await tree.dispatch.admit_input(root.id, event_type=ET.USER, content="Take off. Ship the feature.", actor="user")

  with test_task_execution.make_api_client(cfg, session_blocks, tree) as client:
    resp = client.post(
        "/api/internal/delegate",
        json=conftest.delegate_payload(worker.id, repo),
        headers=test_task_execution.OP_HEADERS)
  assert resp.status_code == 403
  assert "not a manager" in resp.json()["detail"]
  assert builds == []


@pytest.mark.asyncio
async def test_foreign_run_token_cannot_delegate(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, repo: pathlib.Path) -> None:
  """A run token bound to one node cannot create a worker under a different
    node: the worker-leaf constraint refuses the foreign identity."""
  cfg, session_blocks, tree, root, child = await make_tree(tmp_path, monkeypatch)
  builds = conftest.install_scripted_backends(monkeypatch, [], conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)
  await tree.dispatch.admit_input(root.id, event_type=ET.USER, content="Take off. Ship the feature.", actor="user")
  agents_child = await tree.create_task(
      request_id="agent-leaf",
      task_parent_id=child.id,
      profile="worker",
      task=models.TaskSpec(goal="agent leaf"),
      name="AL",
      backend=None,
      caller="operator")
  await tree.runs.register_run_locked(models.RunRecord(id="agent-run", session_id=agents_child.id, kind="work"))
  import subprocess

  from src.runtime import runs
  proc = subprocess.Popen(["/bin/sleep", "30"])
  pair = runs.read_pid_stat(proc.pid)
  await tree.runs.record_launch(agents_child.id, "agent-run", pid=proc.pid, pid_start=pair[0])
  token = run_token.sign_run_token(
      run_token.RunTokenClaims(session_id=agents_child.id, run_id="agent-run", agent="worker"), "op-secret")
  with test_task_execution.make_api_client(cfg, session_blocks, tree) as client:
    resp = client.post(
        "/api/internal/delegate",
        json=conftest.delegate_payload(child.id, repo),
        headers={"Authorization": f"Bearer {token}"})
  proc.terminate()
  assert resp.status_code == 403
  assert "task directly under its own" in resp.json()["detail"]
  assert builds == []


@pytest.mark.asyncio
async def test_verify_exemption_on_the_v2_route_and_launch(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, repo: pathlib.Path) -> None:
  """A read-only verify delegation needs no user message on the real
    v2 route and still launches (and runs) with no user instruction anywhere —
    while a repo task type under the same tree stays blocked."""
  cfg, session_blocks, tree, _root, child = await make_tree(tmp_path, monkeypatch)
  builds = conftest.install_scripted_backends(
      monkeypatch, [test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("verdict: no")])],
      conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)

  with test_task_execution.make_api_client(cfg, session_blocks, tree) as client:
    verify = client.post(
        "/api/internal/delegate",
        json=conftest.delegate_payload(child.id, repo, task_type="verify"),
        headers=test_task_execution.OP_HEADERS)
    assert verify.status_code == 200, verify.text
    verify_leaf = verify.json()["session_id"]
    verify_run = verify.json()["run_id"]

    blocked = client.post(
        "/api/internal/delegate",
        json=conftest.delegate_payload(child.id, repo),
        headers=test_task_execution.OP_HEADERS)
    assert blocked.status_code == 403

    # The verify run launched and settled on the API loop that scheduled it.
    _run, outcome = await test_task_execution.wait_for_terminal_run(tree, verify_leaf, verify_run)
    assert outcome == "success"
  # The verify child is repo-less; the blocked repo delegation created nothing.
  leaves = [m for m in (await tree._get_index()).metas.values() if m.task_parent_id == child.id]
  assert [m.id for m in leaves] == [verify_leaf]
  assert len(builds) == 1


async def register_active_run(tree, session_id: str, run_id: str, kind: str = "manager_turn") -> None:
  """Pin one active, launched Run on *session_id*: the identity its run
    token stands for (registered, no terminal fact, launch identity pinned)."""
  await tree.runs.register_run_locked(models.RunRecord(id=run_id, session_id=session_id, kind=kind))
  await tree.runs.record_launch(session_id, run_id, pid=424242, pid_start=f"ps-{run_id}")


@pytest.mark.asyncio
async def test_agent_run_token_delegates_verify_without_a_takeoff(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, repo: pathlib.Path) -> None:
  """The CLI's real credential — a run token — delegating a read-only verify
    needs no user message on the chain: with no user instruction anywhere, the delegation
    returns 200, creates a repo-less verify child, and its run launches and
    settles (the agent-creation check reads the same verify exemption the
    route and the launch read)."""
  cfg, session_blocks, tree, _root, child = await make_tree(tmp_path, monkeypatch)
  builds = conftest.install_scripted_backends(
      monkeypatch, [test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("verdict: yes")])],
      conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)
  await register_active_run(tree, child.id, "child-run")

  with test_task_execution.make_api_client(cfg, session_blocks, tree) as client:
    verify = client.post(
        "/api/internal/delegate",
        json=conftest.delegate_payload(child.id, repo, task_type="verify"),
        headers=conftest.agent_headers(child.id, "child-run"))
    assert verify.status_code == 200, verify.text
    leaf_id, run_id = verify.json()["session_id"], verify.json()["run_id"]
    leaf = await tree.load_meta(leaf_id)
    assert leaf is not None and leaf.profile == "worker"
    assert leaf.task_parent_id == child.id
    assert leaf.task is not None
    assert leaf.task.task_type == "verify" and leaf.task.repo_path is None
    _run, outcome = await test_task_execution.wait_for_terminal_run(tree, leaf_id, run_id)
  assert outcome == "success"
  assert len(builds) == 1
