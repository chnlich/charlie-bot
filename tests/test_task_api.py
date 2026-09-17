"""API-layer tests for the task-tree foundation endpoints and caller identity."""

from __future__ import annotations

import subprocess
from datetime import UTC, datetime
from pathlib import Path

import pytest
import pytest_asyncio
from conftest import OPUS_BACKEND_ID, stub_credentials
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.api import sessions as sessions_api
from src.api import threads as threads_api
from src.api.deps import get_config, get_config_on_loop, get_run_store, get_session_manager, get_task_manager
from src.core import event_types as ET
from src.core.models import PatchSessionTaskRequest, RunRecord
from src.core.run_token import CallerIdentity, RunTokenClaims, sign_run_token
from src.core.sessions import SessionManager
from src.core.task_sessions import TaskTreeManager

OP = CallerIdentity(kind="operator")


@pytest_asyncio.fixture
async def task_env(tmp_path: Path):
  from conftest import make_home_config
  cfg = make_home_config(tmp_path)
  session_mgr = SessionManager(cfg)
  task_mgr = TaskTreeManager(cfg, session_mgr)
  return cfg, session_mgr, task_mgr


def make_client(cfg, session_mgr, task_mgr) -> TestClient:
  app = FastAPI()
  app.include_router(sessions_api.router, prefix="/api/sessions")
  app.include_router(threads_api.router, prefix="/api/threads")
  app.dependency_overrides[get_config] = lambda: cfg
  app.dependency_overrides[get_config_on_loop] = lambda: cfg
  app.dependency_overrides[get_session_manager] = lambda: session_mgr
  app.dependency_overrides[get_task_manager] = lambda: task_mgr
  app.dependency_overrides[get_run_store] = lambda: task_mgr.runs
  return TestClient(app)


async def seed_tree(task_mgr: TaskTreeManager) -> dict[str, str]:
  root = await task_mgr.create_task(
      request_id="root", task_parent_id=None, profile="manager", task=None, name="Root",
      backend=None, caller="operator")
  worker = await task_mgr.create_task(
      request_id="w1", task_parent_id=root.id, profile="worker", task=None, name="W1",
      backend=None, caller="operator")
  return {"root": root.id, "worker": worker.id}


# ---------------------------------------------------------------------------
# v2 create, tree, detail, runs
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_v2_create_tree_detail_and_runs(task_env) -> None:
  cfg, session_mgr, task_mgr = task_env
  ids = await seed_tree(task_mgr)
  with make_client(cfg, session_mgr, task_mgr) as client:
    # v2 create requires request_id.
    assert client.post(
        "/api/sessions/", json={"task_parent_id": ids["root"], "profile": "worker"}).status_code == 400

    created = client.post("/api/sessions/", json={
        "request_id": "api-w2", "task_parent_id": ids["root"], "profile": "worker", "name": "W2",
    })
    assert created.status_code == 200
    assert created.json()["schema_version"] == 2 and created.json()["profile"] == "worker"
    replay = client.post("/api/sessions/", json={
        "request_id": "api-w2", "task_parent_id": ids["root"], "profile": "worker", "name": "W2",
    })
    assert replay.json()["id"] == created.json()["id"]

    tree = client.get("/api/sessions/tree")
    assert tree.status_code == 200
    body = tree.json()
    assert body["tree_revision"]
    root_row = next(r for r in body["items"] if r["id"] == ids["root"])
    assert root_row["child_count"] == 2 and root_row["task_state"] == "open"

    detail = client.get(f"/api/sessions/{ids['worker']}")
    assert detail.status_code == 200
    assert detail.json()["ancestors"] == [{"id": ids["root"], "name": "Root"}]

    # Retry is stable per request id and visible in GET runs.
    await task_mgr.runs.register_run(RunRecord(id="run-1", session_id=ids["worker"]))
    retry = client.post(f"/api/sessions/{ids['worker']}/retry",
                        json={"request_id": "retry-1", "run_id": "run-1"})
    assert retry.status_code == 200
    retry_body = retry.json()
    assert retry_body["session_id"] == ids["worker"]
    replayed = client.post(f"/api/sessions/{ids['worker']}/retry",
                           json={"request_id": "retry-1", "run_id": "run-1"})
    assert replayed.json()["run_id"] == retry_body["run_id"]

    runs = client.get(f"/api/sessions/{ids['worker']}/runs")
    assert runs.status_code == 200
    assert {r["id"] for r in runs.json()["items"]} == {"run-1", retry_body["run_id"]}

    missing = client.get("/api/sessions/no-such/runs")
    assert missing.status_code == 404


@pytest.mark.asyncio
async def test_one_click_root_create_empty_goal_is_a_valid_task(task_env) -> None:
    """The primary New Task action's exact body: one root manager, empty task
    instructions, no name/backend override — the server's default title and
    backend resolution apply, the empty goal is accepted, and a replayed
    request returns the same node."""
    cfg, session_mgr, task_mgr = task_env
    with make_client(cfg, session_mgr, task_mgr) as client:
        body = {
            "request_id": "one-click-root",
            "task_parent_id": None,
            "profile": "manager",
            "task": {"goal": "", "acceptance": [], "context_refs": []},
        }
        created = client.post("/api/sessions/", json=body)
        assert created.status_code == 200
        meta = created.json()
        assert meta["schema_version"] == 2 and meta["profile"] == "manager"
        assert meta["task_parent_id"] is None
        assert meta["task"]["goal"] == ""
        assert meta["name"] == "New manager task", "the server default title, renameable later"
        assert meta["backend"] == OPUS_BACKEND_ID, "the existing default backend resolution"
        replay = client.post("/api/sessions/", json=body)
        assert replay.status_code == 200
        assert replay.json()["id"] == meta["id"], "the replay contract keeps one node per action"
        detail = client.get(f"/api/sessions/{meta['id']}")
        assert detail.status_code == 200
        assert detail.json()["task_state"] == "open"


@pytest.mark.asyncio
async def test_explicit_backend_create_records_the_choice_and_replays_keep_it(tmp_path: Path) -> None:
  """The toolbar's model choice: the create POST carries the selected backend, the node
  records it, a replayed request returns the original product even when the retried body
  names another model (no silent switch, no second node), and an unknown id is a 400."""
  from conftest import CODEX_BACKEND_OPTION, OPUS_BACKEND_OPTION

  from src.core.config import CharlieBotConfig

  cfg = CharlieBotConfig(
      charliebot_home=tmp_path / "charliebot-home",
      backends={"options": [OPUS_BACKEND_OPTION, CODEX_BACKEND_OPTION],
                "preference": [OPUS_BACKEND_ID]})
  session_mgr = SessionManager(cfg)
  task_mgr = TaskTreeManager(cfg, session_mgr)
  with make_client(cfg, session_mgr, task_mgr) as client:
    body = {
        "request_id": "toolbar-choice",
        "task_parent_id": None,
        "profile": "manager",
        "task": {"goal": "", "acceptance": [], "context_refs": []},
        "backend": "codex-o3",
    }
    created = client.post("/api/sessions/", json=body)
    assert created.status_code == 200
    assert created.json()["backend"] == "codex-o3", "the selected backend lands on the node"

    replay = client.post("/api/sessions/", json={**body, "backend": OPUS_BACKEND_ID})
    assert replay.status_code == 200
    assert replay.json()["id"] == created.json()["id"]
    assert replay.json()["backend"] == "codex-o3", \
        "the replayed action returns its original product; the choice never silently switches"

    refused = client.post("/api/sessions/", json={**body, "request_id": "other-choice",
                                                  "backend": "no-such-model"})
    assert refused.status_code == 400
    assert "not a recognized backend id" in refused.json()["detail"]


@pytest.mark.asyncio
async def test_stale_tree_pagination_returns_409(task_env) -> None:
  cfg, session_mgr, task_mgr = task_env
  await seed_tree(task_mgr)
  await task_mgr.create_task(
      request_id="root-2", task_parent_id=None, profile="manager", task=None, name="Root2",
      backend=None, caller=OP)
  with make_client(cfg, session_mgr, task_mgr) as client:
    first = client.get("/api/sessions/tree", params={"limit": 1})
    cursor = first.json()["next_cursor"]
    assert cursor
    # The tree changes under the cursor.
    client.post("/api/sessions/", json={"request_id": "root-3", "profile": "manager", "name": "Root3"})
    stale = client.get("/api/sessions/tree", params={"limit": 1, "cursor": cursor})
    assert stale.status_code == 409
    assert "changed during pagination" in stale.json()["detail"]["message"]


@pytest.mark.asyncio
async def test_patch_metadata_and_permanent_delete_blockers(task_env) -> None:
  cfg, session_mgr, task_mgr = task_env
  ids = await seed_tree(task_mgr)
  with make_client(cfg, session_mgr, task_mgr) as client:
    # Name-only PATCH keeps the legacy rename contract.
    renamed = client.patch(f"/api/sessions/{ids['worker']}", json={"name": "Renamed"})
    assert renamed.status_code == 200 and renamed.json()["name"] == "Renamed"

    patched = client.patch(f"/api/sessions/{ids['worker']}", json={
        "task": {"goal": "ship the tree", "acceptance": ["tests pass"]},
        "presentation": "shown",
        "automation_paused": True,
    })
    assert patched.status_code == 200
    body = patched.json()
    assert body["task"]["goal"] == "ship the tree"
    assert body["automation_paused"] is True and body["presentation"] == "shown"

    # Reparent to a worker target is a 400; to a missing task a 404.
    bad = client.patch(f"/api/sessions/{ids['worker']}", json={"task_parent_id": ids["worker"]})
    assert bad.status_code == 409  # its own subtree
    missing = client.patch(f"/api/sessions/{ids['worker']}", json={"task_parent_id": "ghost"})
    assert missing.status_code == 404

    # Permanent delete is blocked by the child run record.
    await task_mgr.runs.register_run(RunRecord(id="run-blk", session_id=ids["worker"]))
    blocked = client.delete(f"/api/sessions/{ids['worker']}/permanent")
    assert blocked.status_code == 409
    assert any("run record" in b for b in blocked.json()["detail"]["blockers"])


@pytest.mark.asyncio
async def test_run_cancel_route_and_legacy_thread_alias_route(task_env) -> None:
  cfg, session_mgr, task_mgr = task_env
  ids = await seed_tree(task_mgr)
  proc = subprocess.Popen(["/bin/sleep", "30"])
  try:
    from src.core.runs import read_pid_stat
    pair = read_pid_stat(proc.pid)
    assert pair is not None
    await task_mgr.runs.register_run(
        RunRecord(id="run-live", session_id=ids["worker"], pid=proc.pid, pid_start=pair[0],
                  started_at=datetime.now(UTC)))
    with make_client(cfg, session_mgr, task_mgr) as client:
      # The v2 cancel route stops the specific owned process.
      cancelled = client.post(f"/api/sessions/{ids['worker']}/runs/run-live/cancel",
                              json={"request_id": "c1"})
      assert cancelled.status_code == 200
      assert cancelled.json() == {"run_id": "run-live", "stop_requested": True, "outcome": "interrupted"}
      assert proc.poll() is not None

      # The legacy thread cancel route resolves the v2 alias to the same Run,
      # and never writes a legacy ThreadMetadata status copy.
      proc2 = subprocess.Popen(["/bin/sleep", "30"])
      try:
        pair2 = read_pid_stat(proc2.pid)
        assert pair2 is not None
        await task_mgr.runs.register_run(
            RunRecord(id="run-live-2", session_id=ids["worker"], pid=proc2.pid, pid_start=pair2[0],
                      started_at=datetime.now(UTC)))
        legacy = client.post(f"/api/threads/{ids['worker']}/threads/run-live-2/cancel")
        assert legacy.status_code == 200
        assert legacy.json()["outcome"] == "interrupted"
        assert proc2.poll() is not None
        assert not (cfg.sessions_dir / ids["worker"] / "threads" / "run-live-2").exists()
        thread_status = await session_mgr.get_session(ids["worker"])
        assert thread_status is not None
        assert not (cfg.sessions_dir / ids["worker"] / "threads" / "run-live-2" / "metadata.json").exists()
      finally:
        if proc2.poll() is None:
          proc2.kill()
  finally:
    if proc.poll() is None:
      proc.kill()


# ---------------------------------------------------------------------------
# Caller identity: operator vs run-token agent
# ---------------------------------------------------------------------------


def agent_headers(claims: RunTokenClaims) -> dict[str, str]:
  key = "op-secret"
  stub_credentials({"charliebot": {"access_key": key}})
  token = sign_run_token(claims, key)
  return {"Authorization": f"Bearer {token}"}


async def register_agent_run(task_mgr: TaskTreeManager, claims: RunTokenClaims) -> RunRecord:
  """Register the launched Run the test's agent token binds to.

  The run credential is accepted only after the launch callback persisted
  (pid, pid_start) on the Run, so the fixture lands the identity like the
  real on_spawn callback does.
  """
  record = await task_mgr.runs.register_run(
      RunRecord(id=claims.run_id, session_id=claims.session_id, kind="work"))
  await task_mgr.runs.record_launch(
      claims.session_id, claims.run_id,
      pid=40000 + (abs(hash(claims.run_id)) % 20000), pid_start="ps-" + claims.run_id[:8])
  return record


@pytest.mark.asyncio
async def test_agent_run_token_creates_own_children_under_its_own_task(task_env) -> None:
  """A valid active-Run token organizes its own manager task: a logical manager
  child needs no user authorization, a worker child rides the takeoff gate
  (present here), and any foreign parent is refused."""
  cfg, session_mgr, task_mgr = task_env
  ids = await seed_tree(task_mgr)
  stub_credentials({"charliebot": {"access_key": "op-secret"}})
  claims = RunTokenClaims(session_id=ids["root"], run_id="agent-run-1", agent="worker-alpha")
  await register_agent_run(task_mgr, claims)
  # The agent's own manager task carries the operator's authorization.
  await task_mgr.events.append(ids["root"], {
      "id": "user-1", "type": "user", "timestamp": datetime.now(UTC).isoformat(),
      "actor": "user", "source_session_id": ids["root"], "content": "take off",
  })

  with make_client(cfg, session_mgr, task_mgr) as client:
    # A valid active-Run token may create a worker directly under its own manager task.
    ok = client.post(
        "/api/sessions/",
        json={"request_id": "agent-w", "task_parent_id": ids["root"], "profile": "worker", "name": "AgentW"},
        headers=agent_headers(claims))
    assert ok.status_code == 200, ok.text
    assert ok.json()["profile"] == "worker"

    # ...and a logical manager child under its own task (no extra approval),
    # but not under somebody else's task.
    manager_try = client.post(
        "/api/sessions/",
        json={"request_id": "agent-m", "task_parent_id": ids["root"], "profile": "manager"},
        headers=agent_headers(claims))
    assert manager_try.status_code == 200, manager_try.text
    assert manager_try.json()["profile"] == "manager"

    foreign_try = client.post(
        "/api/sessions/",
        json={"request_id": "agent-f", "task_parent_id": ids["worker"], "profile": "worker"},
        headers=agent_headers(claims))
    assert foreign_try.status_code == 403

    foreign_manager_try = client.post(
        "/api/sessions/",
        json={"request_id": "agent-fm", "task_parent_id": ids["worker"], "profile": "manager"},
        headers=agent_headers(claims))
    assert foreign_manager_try.status_code == 403

    # User-only structural mutations are 403 for an agent caller.
    patch_try = client.patch(
        f"/api/sessions/{ids['worker']}", json={"automation_paused": True},
        headers=agent_headers(claims))
    assert patch_try.status_code == 403

    retry_try = client.post(
        f"/api/sessions/{ids['worker']}/retry", json={"request_id": "r", "run_id": "run-1"},
        headers=agent_headers(claims))
    assert retry_try.status_code == 403


@pytest.mark.asyncio
async def test_invalid_or_ended_run_tokens_fail_closed(task_env) -> None:
  cfg, session_mgr, task_mgr = task_env
  ids = await seed_tree(task_mgr)
  stub_credentials({"charliebot": {"access_key": "op-secret"}})
  with make_client(cfg, session_mgr, task_mgr) as client:
    # Forged signature: 401, even with a valid operator cookie riding along.
    bad = client.post(
        "/api/sessions/",
        json={"request_id": "x", "task_parent_id": ids["root"], "profile": "worker"},
        headers={"Authorization": "Bearer forged-token", "Cookie": "charliebot_access_key=op-secret"})
    assert bad.status_code == 401

    # A validly signed token whose Run has ended is expired.
    await task_mgr.runs.register_run(RunRecord(id="run-done", session_id=ids["worker"]))
    await task_mgr.runs.record_finish(ids["worker"], "run-done", "success")
    ended = client.post(
        "/api/sessions/",
        json={"request_id": "y", "task_parent_id": ids["root"], "profile": "worker"},
        headers=agent_headers(RunTokenClaims(session_id=ids["worker"], run_id="run-done", agent="a")))
    assert ended.status_code == 401

    # A token for a run that was never registered is equally rejected.
    ghost = client.post(
        "/api/sessions/",
        json={"request_id": "g", "task_parent_id": ids["root"], "profile": "worker"},
        headers=agent_headers(RunTokenClaims(session_id=ids["worker"], run_id="ghost", agent="a")))
    assert ghost.status_code == 401

    # Missing signing key is an explicit error for run-token use: the signed
    # token is presented while the credentials carry no signing key at all.
    token = sign_run_token(
        RunTokenClaims(session_id=ids["worker"], run_id="run-done", agent="a"), "op-secret")
    stub_credentials({"charliebot": {"access_key": ""}})
    no_key = client.post(
        "/api/sessions/",
        json={"request_id": "nk", "task_parent_id": ids["root"], "profile": "worker"},
        headers={"Authorization": f"Bearer {token}"})
    assert no_key.status_code == 401
    assert "no signing key" in no_key.json()["detail"]


@pytest.mark.asyncio
async def test_payload_actor_is_never_identity(task_env) -> None:
  """A self-reported actor claim grants nothing: the structured bodies reject the unknown
  field outright, and an agent caller stays agent-scoped whatever it claims."""
  cfg, session_mgr, task_mgr = task_env
  ids = await seed_tree(task_mgr)
  stub_credentials({"charliebot": {"access_key": "op-secret"}})
  claims = RunTokenClaims(session_id=ids["root"], run_id="agent-run-1", agent="worker-alpha")
  await register_agent_run(task_mgr, claims)
  with make_client(cfg, session_mgr, task_mgr) as client:
    forged = client.patch(
        f"/api/sessions/{ids['worker']}",
        json={"automation_paused": True, "actor": "user"},
        headers=agent_headers(claims))
    assert forged.status_code == 422  # the structured body has no actor field to claim

    response = client.patch(
        f"/api/sessions/{ids['worker']}",
        json={"automation_paused": True},
        headers=agent_headers(claims))
    assert response.status_code == 403  # identity is the token's, never the payload's


# ---------------------------------------------------------------------------
# Authorization: nearest real user instruction wins
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_agent_worker_creation_uses_nearest_user_ancestor_gate(task_env) -> None:
  from src.core.control_events import build_control_event
  from src.core.event_types import AGENT_MESSAGE, USER

  cfg, session_mgr, task_mgr = task_env
  stub_credentials({"charliebot": {"access_key": "op-secret"}})
  root = await task_mgr.create_task(
      request_id="root", task_parent_id=None, profile="manager", task=None, name="Root",
      backend=None, caller="operator")
  mid = await task_mgr.create_task(
      request_id="mid", task_parent_id=root.id, profile="manager", task=None, name="Mid",
      backend=None, caller="operator")
  claims = RunTokenClaims(session_id=mid.id, run_id="agent-run-1", agent="worker-alpha")
  await register_agent_run(task_mgr, claims)

  async def append(session_id: str, event: dict) -> None:
    await task_mgr.events.append(session_id, event)

  # The root has a real user instruction; mid has only an agent message.
  await append(root.id, build_control_event(USER, actor="user", source_session_id=root.id,
                                            content="take off", role="user"))
  await append(mid.id, build_control_event(AGENT_MESSAGE, actor="agent", source_session_id=root.id,
                                           content="carry on"))

  with make_client(cfg, session_mgr, task_mgr) as client:
    ok = client.post(
        "/api/sessions/",
        json={"request_id": "borrowed", "task_parent_id": mid.id, "profile": "worker"},
        headers=agent_headers(claims))
    assert ok.status_code == 200, ok.text  # borrowed from the nearest user-instruction ancestor

  # A more recent local real user instruction without take off blocks the borrow.
  await append(mid.id, build_control_event(USER, actor="user", source_session_id=mid.id,
                                           content="hold on", role="user"))
  with make_client(cfg, session_mgr, task_mgr) as client:
    blocked = client.post(
        "/api/sessions/",
        json={"request_id": "blocked", "task_parent_id": mid.id, "profile": "worker"},
        headers=agent_headers(claims))
    assert blocked.status_code == 403
    assert "pre take off" in blocked.json()["detail"] or "take off" in blocked.json()["detail"]


@pytest.mark.asyncio
async def test_agent_messages_and_cron_inputs_never_mint_authorization(task_env) -> None:
  from src.core.control_events import build_control_event
  from src.core.event_types import AGENT_MESSAGE

  cfg, session_mgr, task_mgr = task_env
  stub_credentials({"charliebot": {"access_key": "op-secret"}})
  root = await task_mgr.create_task(
      request_id="root", task_parent_id=None, profile="manager", task=None, name="Root",
      backend=None, caller="operator")
  claims = RunTokenClaims(session_id=root.id, run_id="agent-run-1", agent="worker-alpha")
  await register_agent_run(task_mgr, claims)
  await task_mgr.events.append(root.id, build_control_event(
      AGENT_MESSAGE, actor="agent", source_session_id=root.id, content="take off now please"))

  with make_client(cfg, session_mgr, task_mgr) as client:
    blocked = client.post(
        "/api/sessions/",
        json={"request_id": "no-auth", "task_parent_id": root.id, "profile": "worker"},
        headers=agent_headers(claims))
    assert blocked.status_code == 403


@pytest.mark.asyncio
async def test_legacy_alias_cancel_keeps_agent_own_run_scope(task_env) -> None:
  """The legacy thread cancel route resolves aliases under the v2 route's caller scope."""
  cfg, session_mgr, task_mgr = task_env
  ids = await seed_tree(task_mgr)
  stub_credentials({"charliebot": {"access_key": "op-secret"}})
  own = RunTokenClaims(session_id=ids["worker"], run_id="run-own", agent="worker-alpha")
  await task_mgr.runs.register_run(RunRecord(id="run-own", session_id=ids["worker"], kind="work"))
  await task_mgr.runs.record_launch(ids["worker"], "run-own", pid=424242, pid_start="ps-own")
  await task_mgr.runs.register_run(RunRecord(id="run-foreign", session_id=ids["root"], kind="work"))

  with make_client(cfg, session_mgr, task_mgr) as client:
    # An agent may stop its own bound run through the legacy alias entry.
    ok = client.post(f"/api/threads/{ids['worker']}/threads/run-own/cancel", headers=agent_headers(own))
    assert ok.status_code == 200, ok.text
    # The launched run's identity is observed at stop: the fixture's process
    # is already gone, so the stop records the durable interrupted fact.
    assert ok.json() == {"run_id": "run-own", "stop_requested": True, "outcome": "interrupted"}

    # ...but not another session's run, even a validly signed one. A second
    # launched run backs the token: the first stop finalized run-own, and a
    # finished run's credential is no longer accepted.
    other = RunTokenClaims(session_id=ids["worker"], run_id="run-own-2", agent="worker-alpha")
    await task_mgr.runs.register_run(RunRecord(id="run-own-2", session_id=ids["worker"], kind="work"))
    await task_mgr.runs.record_launch(ids["worker"], "run-own-2", pid=424243, pid_start="ps-own2")
    denied = client.post(f"/api/threads/{ids['root']}/threads/run-foreign/cancel", headers=agent_headers(other))
    assert denied.status_code == 403

    # The operator cookie-less CLI identity keeps full scope.
    operator = client.post(f"/api/threads/{ids['root']}/threads/run-foreign/cancel",
                           headers={"Authorization": "Bearer op-secret"})
    assert operator.status_code == 200


@pytest.mark.asyncio
async def test_agent_run_token_cannot_use_the_legacy_create_shape(task_env) -> None:
  """Run credentials create only their own worker child; the v1 create shape is operator scope."""
  cfg, session_mgr, task_mgr = task_env
  ids = await seed_tree(task_mgr)
  stub_credentials({"charliebot": {"access_key": "op-secret"}})
  claims = RunTokenClaims(session_id=ids["root"], run_id="agent-run-1", agent="worker-alpha")
  await register_agent_run(task_mgr, claims)
  with make_client(cfg, session_mgr, task_mgr) as client:
    denied = client.post("/api/sessions/", json={"name": "Legacy"}, headers=agent_headers(claims))
    assert denied.status_code == 403
    allowed = client.post("/api/sessions/", json={"name": "Legacy"})
    assert allowed.status_code == 200
    assert allowed.json()["profile"] is None  # the legacy v1 shape still works for operators


# ---------------------------------------------------------------------------
# Tree search path projection, ancestor-context rows, pending inputs
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_tree_search_returns_complete_ancestor_paths(task_env) -> None:
  cfg, session_mgr, task_mgr = task_env
  root = await task_mgr.create_task(
      request_id="root", task_parent_id=None, profile="manager", task=None, name="Atlas",
      backend=None, caller="operator")
  mid = await task_mgr.create_task(
      request_id="mid", task_parent_id=root.id, profile="manager",
      task=None, name="Feature", backend=None, caller="operator")
  leaf = await task_mgr.create_task(
      request_id="leaf", task_parent_id=mid.id, profile="worker",
      task=None, name="Fold widget", backend=None, caller="operator")
  await task_mgr.patch_task(root.id, PatchSessionTaskRequest(presentation="hidden"), caller=OP)
  with make_client(cfg, session_mgr, task_mgr) as client:
    resp = client.get("/api/sessions/tree/search", params={"q": "fold"})
    assert resp.status_code == 200, resp.text
    body = resp.json()
    assert len(body["items"]) == 1
    hit = body["items"][0]
    assert hit["row"]["id"] == leaf.id
    # The complete path ships with the hit (nearest-first, the detail
    # route's convention): a hidden ancestor is server fact, never something
    # a partial client tree can reconstruct.
    assert [a["id"] for a in hit["ancestors"]] == [mid.id, root.id]
    assert hit["ancestors"][1]["archived"] is True
    # Goal text matches too.
    goal_hit = client.get("/api/sessions/tree/search", params={"q": "atlas"})
    assert [h["row"]["id"] for h in goal_hit.json()["items"]] == [root.id]
    assert client.get("/api/sessions/tree/search", params={"q": ""}).json()["items"] == []


@pytest.mark.asyncio
async def test_tree_page_keeps_archived_ancestor_of_running_descendant(task_env) -> None:
  cfg, session_mgr, task_mgr = task_env
  root = await task_mgr.create_task(
      request_id="root", task_parent_id=None, profile="manager", task=None, name="Root",
      backend=None, caller="operator")
  leaf = await task_mgr.create_task(
      request_id="leaf", task_parent_id=root.id, profile="worker", task=None, name="Leaf",
      backend=None, caller="operator")
  await task_mgr.patch_task(root.id, PatchSessionTaskRequest(presentation="hidden"), caller=OP)
  import os as _os
  await task_mgr.runs.register_run(
      RunRecord(id="run-live", session_id=leaf.id, pid=_os.getpid(), pid_start="1"))
  with make_client(cfg, session_mgr, task_mgr) as client:
    page = client.get("/api/sessions/tree", params={"include_archived": "false"}).json()
    rows = {r["id"]: r for r in page["items"]}
    # The hidden ancestor remains navigable as ancestor context, still archived.
    assert rows[root.id]["archived"] is True
    under = client.get("/api/sessions/tree", params={"parent_id": root.id}).json()
    assert {r["id"] for r in under["items"]} == {leaf.id}
    # With the descendant's run finished (terminal, no attention), the
    # presentation preference wins again and the ancestor drops from its level.
    # A successful delivered worker autoarchives (server facts): the leaf
    # leaves the default projection, and with no active work below it the
    # hidden ancestor's own presentation wins again — the root level empties.
    await task_mgr.dispatch.finish_run(leaf.id, "run-live", outcome="success")
    page2 = client.get("/api/sessions/tree", params={"include_archived": "false"}).json()
    assert page2["items"] == []
    under2 = client.get("/api/sessions/tree", params={"parent_id": root.id}).json()
    assert under2["items"] == []
    # The archived view still preserves the ancestry in place.
    archived_page = client.get("/api/sessions/tree", params={"include_archived": "true"}).json()
    assert {r["id"] for r in archived_page["items"]} == {root.id}


@pytest.mark.asyncio
async def test_pending_task_inputs_endpoint_lists_source_and_text(task_env) -> None:
  cfg, session_mgr, task_mgr = task_env
  root = await task_mgr.create_task(
      request_id="root", task_parent_id=None, profile="manager", task=None, name="Root",
      backend=None, caller="operator")
  await task_mgr.dispatch.admit_input(
      root.id, event_type=ET.USER, content="first instruction", actor="user")
  await task_mgr.dispatch.admit_input(
      root.id, event_type=ET.AGENT_MESSAGE, content="child report body",
      actor="agent", from_session="child-1", from_session_name="Child task")
  with make_client(cfg, session_mgr, task_mgr) as client:
    resp = client.get(f"/api/sessions/{root.id}/task-inputs/pending")
    assert resp.status_code == 200, resp.text
    items = resp.json()["items"]
    assert [i["text"] for i in items] == ["first instruction", "child report body"]
    assert [i["type"] for i in items] == [ET.USER, ET.AGENT_MESSAGE]
    assert items[1]["from_session_name"] == "Child task"
    # The acknowledge route consumes exactly these ids; an unknown id refuses.
    ok = client.post(f"/api/sessions/{root.id}/task-inputs/acknowledge", json={
        "request_id": "ack-1", "input_ids": [items[0]["id"]], "note": "handled in terminal"})
    assert ok.status_code == 200, ok.text
    after = client.get(f"/api/sessions/{root.id}/task-inputs/pending").json()["items"]
    assert [i["id"] for i in after] == [items[1]["id"]]
    bad = client.post(f"/api/sessions/{root.id}/task-inputs/acknowledge", json={
        "request_id": "ack-2", "input_ids": ["not-an-input"]})
    assert bad.status_code == 409
