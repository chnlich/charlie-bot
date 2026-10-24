"""Logical-manager authorization boundary on the real entry points.

The user's correction: a logical manager session is agent-organizable — a valid
active manager Run creates logical manager children under its own open task
through the ordinary task-create API/CLI at any depth, and archives its own
task through the completion owner, both with no user
message anywhere on the tree. Implementation stays gated: worker children ride
the runtime gate (a real user message or a thread summon on the open task
chain), agent relay text and child reports never count as user requests, and
caller scope (own parent, manager parent, open ancestry, replay identity) is
enforced exactly as before.
Every scenario here runs the actual HTTP routes against a synthetic instance
with scripted backends; no real model or provider takes part.
"""

from __future__ import annotations

import datetime
import os
import pathlib
import subprocess

import conftest
import pytest

from src.infra import event_types as ET
from src.infra import models
from src.runtime import control_events, runs, task_sessions
from tests import test_task_execution


def live_run_identity() -> tuple[int, str]:
  """A real live process identity (the verified-live precondition of an own-run request)."""
  pid = os.getpid()
  pair = runs.read_pid_stat(pid)
  assert pair is not None
  return pid, pair[0]


async def register_live_manager_run(tree: task_sessions.TaskTreeManager, session_id: str, run_id: str) -> None:
  """Pin one live manager_turn Run: the node's own coordination Run whose
  identity backs its completion credential."""
  pid, pid_start = live_run_identity()
  await tree.runs.register_run_locked(
      models.RunRecord(
          id=run_id,
          session_id=session_id,
          kind="manager_turn",
          pid=pid,
          pid_start=pid_start,
          started_at=datetime.datetime.now(datetime.UTC)))


async def manager_tree(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch):
  """One synthetic instance: root manager, executor installed, NO user input anywhere."""
  return await test_task_execution.build_manager_root(tmp_path, monkeypatch)


async def create_child_via_api(client, parent_id: str, token: dict, request_id: str, profile: str = "manager"):
  return client.post(
      "/api/sessions/",
      json={
          "request_id": request_id,
          "task_parent_id": parent_id,
          "profile": profile,
      },
      headers=token)


# ---------------------------------------------------------------------------
# Creation without takeoff: durable, stable, any depth
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_manager_child_creation_needs_no_takeoff_and_replays_stably(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """With a valid active Run token and NO user authorization anywhere, the
  public create API makes a logical manager child under the caller's own open
  task; the replayed request returns the original product; the node is durable
  on disk with the agent as the creation fact's actor."""
  cfg, session_blocks, tree, root = await manager_tree(tmp_path, monkeypatch)
  await tree.runs.register_run_locked(models.RunRecord(id="root-run", session_id=root.id, kind="manager_turn"))
  await tree.runs.record_launch(root.id, "root-run", pid=424242, pid_start="ps-root")

  with test_task_execution.make_api_client(cfg, session_blocks, tree) as client:
    body = {"request_id": "child-mgr", "task_parent_id": root.id, "profile": "manager", "name": "Child"}
    first = client.post("/api/sessions/", json=body, headers=conftest.agent_headers(root.id, "root-run"))
    assert first.status_code == 200, first.text
    child = first.json()
    assert child["profile"] == "manager" and child["task_parent_id"] == root.id

    # The replayed identical request returns the original product.
    replay = client.post("/api/sessions/", json=body, headers=conftest.agent_headers(root.id, "root-run"))
    assert replay.status_code == 200, replay.text
    assert replay.json()["id"] == child["id"]

    # No user message anywhere on the tree: the creation fact's actor is the agent.
    created = [e for e in tree.events.load_events(child["id"]) if e["type"] == ET.TASK_CREATED]
    assert len(created) == 1 and created[0]["actor"] == "agent"

  # Durable: a fresh owner over the same home sees the node without a replay.
  fresh = conftest.build_task_tree(cfg, conftest.build_session_blocks(cfg))
  meta = await fresh.load_meta(child["id"])
  assert meta is not None and meta.profile == "manager"


# ---------------------------------------------------------------------------
# Normal own-run archive without authorization prompts
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_own_run_archive_closes_without_takeoff_and_reports_to_ancestor(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A manager's own active Run requests its task's archive with no
  user authorization anywhere: 202 pending_run_finish, then the close lands
  when that Run finishes successfully. The parent stays open — a child's
  completion never finishes a long-lived ancestor."""
  cfg, session_blocks, tree, root = await manager_tree(tmp_path, monkeypatch)
  child = await tree.create_task(
      request_id="child",
      task_parent_id=root.id,
      profile="manager",
      task=models.TaskSpec(goal="feature"),
      name="Feature",
      backend=None,
      caller=conftest.OPERATOR)
  await register_live_manager_run(tree, child.id, "child-owner-run")
  blocked_manager = await conftest.create_task(tree, parent=root.id, request_id="blocked-manager")
  await register_live_manager_run(tree, blocked_manager.id, "blocked-owner-run")
  await conftest.create_task(tree, parent=blocked_manager.id, request_id="open-child")

  with test_task_execution.make_api_client(cfg, session_blocks, tree) as client:
    pending = client.post(
        f"/api/sessions/{root.id}/archive",
        json={
            "request_id": "wrong-task",
            "summary": "coordination complete",
            "result_refs": ["report:feature-coordinated"],
            "run_ids": ["child-owner-run"]
        },
        headers=conftest.agent_headers(child.id, "child-owner-run"))
    assert pending.status_code == 403

    blocked = client.post(
        f"/api/sessions/{blocked_manager.id}/archive",
        json={
            "request_id": "blocked-by-child",
            "summary": "manager report",
            "result_refs": ["report:manager-report"],
        },
        headers=conftest.agent_headers(blocked_manager.id, "blocked-owner-run"))
    assert blocked.status_code == 409
    assert "open descendant" in blocked.json()["detail"]["blockers"][0]
    assert not [e for e in tree.events.load_events(blocked_manager.id) if e["type"] == ET.TASK_CLOSE_REQUESTED]

    pending = client.post(
        f"/api/sessions/{child.id}/archive",
        json={
            "request_id": "close-1",
            "summary": "coordination complete",
            "result_refs": ["report:feature-coordinated"]
        },
        headers=conftest.agent_headers(child.id, "child-owner-run"))
    assert pending.status_code == 202, pending.text
    assert pending.json()["status"] == "pending_run_finish"
    assert tree.task_state(child.id) == "open"

  # The owner Run finishes successfully: the close lands through the existing
  # post-finish re-evaluation and reports to the parent.
  await tree.dispatch.finish_run(child.id, "child-owner-run", outcome="success")
  assert tree.task_state(child.id) == "archived"
  closes = [e for e in tree.events.load_events(child.id) if e["type"] == ET.TASK_CLOSED]
  assert len(closes) == 1 and closes[0]["outcome"] == "archived"
  assert closes[0]["report_to"] == root.id
  reports = [e for e in tree.events.load_events(root.id) if e["type"] == ET.CHILD_REPORT]
  assert len(reports) == 1 and reports[0]["outcome"] == "archived"

  # The ancestor holds its own conditions: it stays open after the child closed.
  assert tree.task_state(root.id) == "open"


# ---------------------------------------------------------------------------
# Implementation stays gated on the same tree
# ---------------------------------------------------------------------------


async def three_manager_tree(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch):
  """Root -> child -> grandchild managers, each created through the public API
  with that node's own Run token, and each holding a registered live Run token
  identity. No user input anywhere."""
  cfg, session_blocks, tree, root = await manager_tree(tmp_path, monkeypatch)
  ids = {"root": root.id}
  parent_id, parent_run = root.id, "root-run"
  await tree.runs.register_run_locked(models.RunRecord(id=parent_run, session_id=parent_id, kind="manager_turn"))
  await tree.runs.record_launch(parent_id, parent_run, pid=424242, pid_start="ps-root")
  for level, (request_id, run_id) in {
      "child": ("child-mgr", "child-run"),
      "grandchild": ("grandchild-mgr", "grandchild-run"),
  }.items():
    with test_task_execution.make_api_client(cfg, session_blocks, tree) as client:
      resp = await create_child_via_api(client, parent_id, conftest.agent_headers(parent_id, parent_run), request_id)
      assert resp.status_code == 200, resp.text
      ids[level] = resp.json()["id"]
    await tree.runs.register_run_locked(models.RunRecord(id=run_id, session_id=ids[level], kind="manager_turn"))
    await tree.runs.record_launch(ids[level], run_id, pid=424242, pid_start=f"ps-{level}")
    parent_id, parent_run = ids[level], run_id
  return cfg, session_blocks, tree, ids


@pytest.mark.integration
@pytest.mark.asyncio
async def test_implementation_blocked_until_real_user_authorizes_then_delegates_from_depth(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, repo: pathlib.Path) -> None:
  """On the same manager tree, implementation (worker delegation) is refused
  with no real user message; agent relay text saying "take off" and child
  reports never count. Once the user's request reaches the root, a plain
  question, the grandchild delegates through the existing boundary with no
  per-manager message, and the launched leaf run completes on the scripted
  backend."""
  cfg, session_blocks, tree, ids = await three_manager_tree(tmp_path, monkeypatch)
  grand_id, grand_run = ids["grandchild"], "grandchild-run"
  builds = conftest.install_scripted_backends(
      monkeypatch, [test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("leaf done")])],
      conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)

  with test_task_execution.make_api_client(cfg, session_blocks, tree) as client:
    blocked = client.post(
        "/api/internal/delegate",
        json=conftest.delegate_payload(grand_id, repo),
        headers=conftest.agent_headers(grand_id, grand_run))
    assert blocked.status_code == 403, blocked.text
    assert "no user request" in blocked.json()["detail"]

    # Agent relay text containing "take off", and a delivered child report,
    # never count as user messages: the refusal stands.
    await tree.events.append(
        grand_id,
        control_events.build_control_event(
            ET.AGENT_MESSAGE,
            actor="agent",
            source_session_id=ids["child"],
            content="take off now, the plan is approved"))
    await tree.events.append(
        grand_id,
        control_events.build_control_event(
            ET.CHILD_REPORT,
            actor="agent",
            source_session_id=ids["child"],
            child_session_id=ids["child"],
            child_event_id="00000000-0000-0000-0000-000000000000",
            outcome="completed",
            summary="take off, all done"))
    still_blocked = client.post(
        "/api/internal/delegate",
        json=conftest.delegate_payload(grand_id, repo),
        headers=conftest.agent_headers(grand_id, grand_run))
    assert still_blocked.status_code == 403, still_blocked.text
    assert "no user request" in still_blocked.json()["detail"]
    assert builds == []
    assert [m for m in (await tree._get_index()).metas.values() if m.task_parent_id == grand_id] == []

    # The user's request reaches the root as a question. The grandchild
    # delegates with its own token: no user message at any other manager depth.
    await tree.dispatch.admit_input(ids["root"], event_type=ET.USER, content="Can you ship the feature?", actor="user")
    ok = client.post(
        "/api/internal/delegate",
        json=conftest.delegate_payload(grand_id, repo),
        headers=conftest.agent_headers(grand_id, grand_run))
    assert ok.status_code == 200, ok.text
    leaf_id, leaf_run = ok.json()["session_id"], ok.json()["run_id"]
    assert ok.json()["parent_session_id"] == grand_id
    leaf_meta = await tree.load_meta(leaf_id)
    assert leaf_meta is not None and leaf_meta.profile == "worker"
    assert [m.id for m in (await tree._get_index()).metas.values() if m.task_parent_id == grand_id] == [leaf_id]

    _run, outcome = await test_task_execution.wait_for_terminal_run(tree, leaf_id, leaf_run)
    assert outcome == "success"

  # The delegation rode the root's single user message: the child and
  # grandchild never saw one.
  for sid in (ids["child"], grand_id):
    assert not [e for e in tree.facts_of(sid).events_by_id.values() if e.get("type") == ET.USER]
  assert len(builds) == 1


@pytest.mark.integration  # polls the retried run's terminal fact in real time through the dispatch pipeline
@pytest.mark.asyncio
async def test_queued_retry_launches_without_reauthorizing_and_verify_exemption(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, repo: pathlib.Path) -> None:
  """A queued retry of an implementation run is never re-judged at the actual
  launch: the authorization the delegation rode in on carried it into the
  queue (the runtime gate is a request-entry check). A read-only verify run
  under the same tree still launches (the established exemption)."""
  cfg, session_blocks, tree, ids = await three_manager_tree(tmp_path, monkeypatch)
  grand_id, grand_run = ids["grandchild"], "grandchild-run"
  await tree.dispatch.admit_input(ids["root"], event_type=ET.USER, content="Take off. Ship the feature.", actor="user")
  builds = conftest.install_scripted_backends(
      monkeypatch, [
          test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("verdict: no")]),
          test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("verdict: yes")])
      ], conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)

  with test_task_execution.make_api_client(cfg, session_blocks, tree) as client:
    ok = client.post(
        "/api/internal/delegate",
        json=conftest.delegate_payload(grand_id, repo),
        headers=conftest.agent_headers(grand_id, grand_run))
    assert ok.status_code == 200, ok.text
    leaf_id, leaf_run = ok.json()["session_id"], ok.json()["run_id"]

  # The work run fails; its explicit retry is a queued run, and it launches
  # with no gate at the launch.
  await tree.dispatch.finish_run(leaf_id, leaf_run, outcome="failed")
  retry = await tree.create_retry(leaf_id, "retry-1", leaf_run)
  assert retry["run_id"]

  tree.dispatch.executor.launch(leaf_id, retry["run_id"])
  _run, outcome = await test_task_execution.wait_for_terminal_run(tree, leaf_id, retry["run_id"])
  assert outcome == "success"
  assert len(builds) == 1  # the retry's own backend build

  # The read-only verify exemption still launches under the same tree.
  verify = await tree.create_task(
      request_id="verify-leaf",
      task_parent_id=grand_id,
      profile="worker",
      task=models.TaskSpec(goal="check the thing", task_type="verify"),
      name="Verify",
      backend=None,
      caller=conftest.OPERATOR)
  verify_run = await tree.runs.register_run_locked(
      models.RunRecord(id="verify-run", session_id=verify.id, kind="work", backend="fake", model="fake-model"))
  observation = await tree.dispatch.executor.launch_and_settle(verify.id, verify_run.id)
  assert observation.withheld is None
  _run, outcome = await test_task_execution.wait_for_terminal_run(tree, verify.id, verify_run.id)
  assert outcome == "success"
  assert len(builds) == 2  # the verify run's own build joins the retry's


# ---------------------------------------------------------------------------
# Caller scope, token identity, replay identity
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_agent_cancels_only_its_own_direct_child_over_http(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The cancel route enforces the agent scope at the boundary: the parent's
  run token cancels its own direct child (200), a non-parent's token is
  refused (403)."""
  cfg, session_blocks, tree, root = await manager_tree(tmp_path, monkeypatch)
  # Each caller's credential rides one launched Run (the caller-identity
  # predicate pins the process identity).
  await tree.runs.register_run_locked(models.RunRecord(id="root-run", session_id=root.id, kind="manager_turn"))
  await tree.runs.record_launch(root.id, "root-run", pid=424242, pid_start="ps-root")
  child = await tree.create_task(
      request_id="child",
      task_parent_id=root.id,
      profile="manager",
      task=models.TaskSpec(goal="feature"),
      name="Feature",
      backend=None,
      caller=conftest.OPERATOR)
  other = await tree.create_task(
      request_id="other",
      task_parent_id=None,
      profile="manager",
      task=models.TaskSpec(goal="other program"),
      name="Other",
      backend=None,
      caller=conftest.OPERATOR)
  await tree.runs.register_run_locked(models.RunRecord(id="other-run", session_id=other.id, kind="manager_turn"))
  await tree.runs.record_launch(other.id, "other-run", pid=424243, pid_start="ps-other")

  with test_task_execution.make_api_client(cfg, session_blocks, tree) as client:
    ok = client.post(
        f"/api/sessions/{child.id}/cancel",
        json={
            "request_id": "agent-cancel",
            "reason": "obsolete delegation"
        },
        headers=conftest.agent_headers(root.id, "root-run"))
    assert ok.status_code == 200, ok.text
    assert ok.json()["task_state"] == "cancelled"
    assert ok.json()["stopped_runs"] == []

    forbidden = client.post(
        f"/api/sessions/{child.id}/cancel",
        json={
            "request_id": "agent-cancel-2",
            "reason": "not my child"
        },
        headers=conftest.agent_headers(other.id, "other-run"))
    assert forbidden.status_code == 403, forbidden.text
    assert "direct child" in str(forbidden.json()["detail"])


@pytest.mark.asyncio
async def test_agent_run_token_cancels_live_worker_over_http(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The worker cancel route returns stopped run outcomes to its parent agent."""
  monkeypatch.setattr(runs, "STOP_EXIT_WAIT_SECONDS", 0.2)
  cfg, session_blocks, tree, root = await manager_tree(tmp_path, monkeypatch)
  await tree.runs.register_run_locked(models.RunRecord(id="root-run", session_id=root.id, kind="manager_turn"))
  await tree.runs.record_launch(root.id, "root-run", pid=424242, pid_start="ps-root")
  worker = await tree.create_task(
      request_id="worker",
      task_parent_id=root.id,
      profile="worker",
      task=models.TaskSpec(goal="stop work"),
      name="Worker",
      backend=None,
      caller=conftest.OPERATOR)
  proc = subprocess.Popen(["/bin/sleep", "60"], start_new_session=True)
  pid, pid_start = conftest.identity_of(proc.pid)
  await tree.runs.register_run_locked(
      models.RunRecord(
          id="worker-run",
          session_id=worker.id,
          kind="work",
          pid=pid,
          pid_start=pid_start,
          started_at=datetime.datetime.now(datetime.UTC)))
  try:
    with test_task_execution.make_api_client(cfg, session_blocks, tree) as client:
      response = client.post(
          f"/api/sessions/{worker.id}/cancel",
          json={
              "request_id": "worker-cancel",
              "reason": "stop"
          },
          headers=conftest.agent_headers(root.id, "root-run"))
    assert response.status_code == 200, response.text
    assert response.json()["task_state"] == "cancelled"
    assert response.json()["stopped_runs"] == [{"run_id": "worker-run", "outcome": "interrupted"}]
    assert proc.poll() is not None
    assert tree.runs.terminal_outcome(tree.runs.load_events_sync(worker.id), "worker-run") == "interrupted"
  finally:
    if proc.poll() is None:
      proc.kill()
    proc.wait(timeout=1)


@pytest.mark.asyncio
async def test_profile_changing_replay_never_bypasses_authorization(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The replay judgment reads the ORIGINAL product's profile: a worker create
  replayed as a manager request stays gated while no user message is on the
  chain, and a manager create replayed as a worker request returns the
  original manager product without minting a worker."""
  cfg, session_blocks, tree, root = await manager_tree(tmp_path, monkeypatch)
  await tree.runs.register_run_locked(models.RunRecord(id="root-run", session_id=root.id, kind="manager_turn"))
  await tree.runs.record_launch(root.id, "root-run", pid=424242, pid_start="ps-root")
  # The operator's create needs no gate; the agent's replays below bind to the
  # same (parent, request_id) node.
  worker = await tree.create_task(
      request_id="w1",
      task_parent_id=root.id,
      profile="worker",
      task=models.TaskSpec(goal="leaf"),
      name="W",
      backend=None,
      caller=conftest.OPERATOR)

  with test_task_execution.make_api_client(cfg, session_blocks, tree) as client:
    token = conftest.agent_headers(root.id, "root-run")

    # No user message on the chain: re-labeling the replay cannot turn the
    # gated worker create into an ungated manager create, and the original
    # shape is gated too.
    for profile in ("manager", "worker"):
      replay = client.post(
          "/api/sessions/", json={
              "request_id": "w1",
              "task_parent_id": root.id,
              "profile": profile
          }, headers=token)
      assert replay.status_code == 403, replay.text
      assert "no user request" in replay.json()["detail"]

    # A manager original replayed with a worker label: the judgment reads the
    # manager product (no gate), returns it unchanged, and mints no worker.
    manager_resp = client.post(
        "/api/sessions/", json={
            "request_id": "m1",
            "task_parent_id": root.id,
            "profile": "manager"
        }, headers=token)
    assert manager_resp.status_code == 200, manager_resp.text
    manager_id = manager_resp.json()["id"]
    replay_as_worker = client.post(
        "/api/sessions/", json={
            "request_id": "m1",
            "task_parent_id": root.id,
            "profile": "worker"
        }, headers=token)
    assert replay_as_worker.status_code == 200, replay_as_worker.text
    assert replay_as_worker.json()["id"] == manager_id
    assert replay_as_worker.json()["profile"] == "manager"

    # Once the user's message reaches the root, the relabeled worker replay
    # returns the original worker product.
    await tree.dispatch.admit_input(root.id, event_type=ET.USER, content="Can you ship the feature?", actor="user")
    relabeled = client.post(
        "/api/sessions/", json={
            "request_id": "w1",
            "task_parent_id": root.id,
            "profile": "manager"
        }, headers=token)
    assert relabeled.status_code == 200, relabeled.text
    assert relabeled.json()["id"] == worker.id
    assert relabeled.json()["profile"] == "worker"

  metas = (await tree._get_index()).metas
  children = [m for m in metas.values() if m.task_parent_id == root.id]
  assert sorted(m.id for m in children) == sorted([worker.id, manager_id])


@pytest.mark.asyncio
async def test_summon_authorizes_a_root_manager_but_a_relayed_agent_message_does_not(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, repo: pathlib.Path) -> None:
  """The delegate route admits a root manager whose log holds only a thread summon: a recorded summon is a
  user request. A log holding only an agent message relayed through the session-message endpoint still has
  no user request, so the same route refuses it."""
  # Refusal: the root's only user-facing input is a relay admitted by the ordinary relay route.
  cfg, session_blocks, tree, root = await manager_tree(tmp_path / "relay", monkeypatch)
  await tree.runs.register_run_locked(models.RunRecord(id="root-run", session_id=root.id, kind="manager_turn"))
  await tree.runs.record_launch(root.id, "root-run", pid=424242, pid_start="ps-root")
  relay_caller = await tree.create_task(
      request_id="relay-caller",
      task_parent_id=None,
      profile="manager",
      task=models.TaskSpec(goal="relay caller"),
      name="Relay Caller",
      backend=None,
      caller=conftest.OPERATOR)
  with test_task_execution.make_api_client(cfg, session_blocks, tree) as client:
    relay = client.post(
        "/api/internal/session-message",
        json={
            "session_id": relay_caller.id,
            "target_session_id": root.id,
            "content": "Take off. Ship the feature."
        })
    assert relay.status_code == 200, relay.text
    blocked = client.post(
        "/api/internal/delegate",
        json=conftest.delegate_payload(root.id, repo),
        headers=conftest.agent_headers(root.id, "root-run"))
  assert blocked.status_code == 403, blocked.text
  assert "no user request" in blocked.json()["detail"]

  # Admission: a fresh root whose only user-facing input is the event a Slack thread summon writes.
  cfg, session_blocks, tree, root = await manager_tree(tmp_path / "summon", monkeypatch)
  await tree.runs.register_run_locked(models.RunRecord(id="root-run", session_id=root.id, kind="manager_turn"))
  await tree.runs.record_launch(root.id, "root-run", pid=424242, pid_start="ps-root")
  conftest.install_scripted_backends(
      monkeypatch, [test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("leaf done")])],
      conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)
  await tree.events.append(
      root.id,
      control_events.build_control_event(
          ET.AGENT_MESSAGE,
          actor="agent",
          source_session_id=root.id,
          content="Slack 线程召唤",
          slack={
              "channel_id": "C123",
              "thread_ts": "1700000000.000100",
              "mention_ts": "1700000000.000200",
          }))
  with test_task_execution.make_api_client(cfg, session_blocks, tree) as client:
    ok = client.post(
        "/api/internal/delegate",
        json=conftest.delegate_payload(root.id, repo),
        headers=conftest.agent_headers(root.id, "root-run"))
  assert ok.status_code == 200, ok.text
  leaf_meta = await tree.load_meta(ok.json()["session_id"])
  assert leaf_meta is not None and leaf_meta.profile == "worker"
