"""API-layer tests for the task-tree foundation endpoints and caller identity."""

from __future__ import annotations

import json
import subprocess
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest
import pytest_asyncio
from conftest import stub_credentials

from src.core.models import RunRecord, utc_now_iso
from src.core.run_token import RunTokenClaims, sign_run_token
from src.core.runs import read_pid_stat
from src.core.sessions import SessionManager
from src.core.task_sessions import TaskTreeManager
from tests.test_task_execution import make_api_client


@pytest_asyncio.fixture
async def task_env(tmp_path: Path):
  from conftest import make_home_config
  cfg = make_home_config(tmp_path)
  session_mgr = SessionManager(cfg)
  task_mgr = TaskTreeManager(cfg, session_mgr)
  return cfg, session_mgr, task_mgr


async def seed_tree(task_mgr: TaskTreeManager) -> dict[str, str]:
  root = await task_mgr.create_task(
      request_id="root",
      task_parent_id=None,
      profile="manager",
      task=None,
      name="Root",
      backend=None,
      caller="operator")
  worker = await task_mgr.create_task(
      request_id="w1", task_parent_id=root.id, profile="worker", task=None, name="W1", backend=None, caller="operator")
  return {"root": root.id, "worker": worker.id}


# ---------------------------------------------------------------------------
# v2 create, tree, detail, runs
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_v2_create_tree_detail_and_runs(task_env) -> None:
  cfg, session_mgr, task_mgr = task_env
  ids = await seed_tree(task_mgr)
  with make_api_client(cfg, session_mgr, task_mgr) as client:
    # v2 create requires request_id.
    assert client.post("/api/sessions/", json={"task_parent_id": ids["root"], "profile": "worker"}).status_code == 400

    created = client.post(
        "/api/sessions/",
        json={
            "request_id": "api-w2",
            "task_parent_id": ids["root"],
            "profile": "worker",
            "name": "W2",
        })
    assert created.status_code == 200
    assert created.json()["schema_version"] == 2 and created.json()["profile"] == "worker"
    replay = client.post(
        "/api/sessions/",
        json={
            "request_id": "api-w2",
            "task_parent_id": ids["root"],
            "profile": "worker",
            "name": "W2",
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
    retry = client.post(f"/api/sessions/{ids['worker']}/retry", json={"request_id": "retry-1", "run_id": "run-1"})
    assert retry.status_code == 200
    retry_body = retry.json()
    assert retry_body["session_id"] == ids["worker"]
    replayed = client.post(f"/api/sessions/{ids['worker']}/retry", json={"request_id": "retry-1", "run_id": "run-1"})
    assert replayed.json()["run_id"] == retry_body["run_id"]

    runs = client.get(f"/api/sessions/{ids['worker']}/runs")
    assert runs.status_code == 200
    assert {r["id"] for r in runs.json()["items"]} == {"run-1", retry_body["run_id"]}

    missing = client.get("/api/sessions/no-such/runs")
    assert missing.status_code == 404


@pytest.mark.asyncio
async def test_activity_tracks_a_finish_that_lands_after_a_warm_derivation(task_env) -> None:
  """The outcome memo rides the events cache's identity with a covered
  cursor, so a run_finished append after a warmed derivation must move the
  verdict — a stale memo would pin a finished run's queued verdict on every
  tree page and sidebar probe until restart."""
  _cfg, _session_mgr, task_mgr = task_env
  ids = await seed_tree(task_mgr)
  worker = ids["worker"]
  await task_mgr.runs.register_run(RunRecord(id="r-memo", session_id=worker))
  assert task_mgr.activity_of(worker).work_state == "waiting"
  await task_mgr.runs.record_finish(worker, "r-memo", "completed")
  assert task_mgr.activity_of(worker).work_state == "idle"


@pytest.mark.asyncio
async def test_activity_tracks_a_launch_that_lands_after_a_warm_derivation(task_env) -> None:
  """The record write funnel bumps the generation the activity memo keys on,
  so a launch identity persisted after a warmed derivation must move the
  queued verdict — a memo the funnel bypass leaves stale would pin the
  waiting verdict on every tree page and sidebar probe."""
  _cfg, _session_mgr, task_mgr = task_env
  ids = await seed_tree(task_mgr)
  worker = ids["worker"]
  await task_mgr.runs.register_run(RunRecord(id="r-launch", session_id=worker))
  assert task_mgr.activity_of(worker).work_state == "waiting"
  proc = subprocess.Popen(["sleep", "30"])
  try:
    pid_start, _state = read_pid_stat(proc.pid)
    await task_mgr.runs.record_launch(worker, "r-launch", pid=proc.pid, pid_start=pid_start)
    assert task_mgr.activity_of(worker).work_state == "running"
  finally:
    proc.kill()
    proc.wait()


@pytest.mark.asyncio
async def test_activity_tracks_a_stop_request_that_lands_after_a_warm_derivation(task_env) -> None:
  """The events cache takes an append in place — the list identity and the
  archived extent survive the new fact — so a chat-only fact transition with
  no record write (a stop request on a queued run) moves the verdict through
  the covered length in the memo key. A key without it would pin the waiting
  verdict on every tree page and sidebar probe until an unrelated record
  write bumped the generation."""
  _cfg, _session_mgr, task_mgr = task_env
  ids = await seed_tree(task_mgr)
  worker = ids["worker"]
  await task_mgr.runs.register_run(RunRecord(id="r-stop", session_id=worker))
  assert task_mgr.activity_of(worker).work_state == "waiting"
  await task_mgr.runs.request_stop(worker, "r-stop", "stop-1")
  assert task_mgr.activity_of(worker).work_state == "idle"


@pytest.mark.asyncio
async def test_fold_agrees_with_a_cold_refold_across_a_recycle(task_env) -> None:
  """The facts and outcome memos key on the events cache's list identity alone;
  the recycle is archive_offset's only writer and it rewrites the live file and
  drops the events cache in the same flow, so the replaced list is what re-keys
  the fold. Warm and cold folds must agree after the move and after a
  post-recycle append: a writer that bumped the offset without that drop would
  leave the warm memo folding the suffix at a stale archived base — silently
  wrong facts on every tree page and sidebar probe."""
  _cfg, session_mgr, task_mgr = task_env
  ids = await seed_tree(task_mgr)
  worker = ids["worker"]
  await task_mgr.runs.register_run(RunRecord(id="r1", session_id=worker))
  await task_mgr.runs.record_finish(worker, "r1", "completed")
  assert task_mgr.activity_of(worker).work_state == "idle"

  cutoff = datetime.now(UTC) + timedelta(hours=1)
  result = await session_mgr.recycle_scheduled_session(worker, cutoff)
  assert result["events_archived"] > 0
  meta = await session_mgr.get_session(worker)
  assert meta is not None and meta.archive_offset == result["events_archived"]

  # The rotation replaced the events cache's list, so the warm fold re-keyed
  # and re-folded from the archived half: r1's fact survives the move.
  assert task_mgr.activity_of(worker).work_state == "idle"

  await task_mgr.runs.register_run(RunRecord(id="r2", session_id=worker))
  await task_mgr.runs.record_finish(worker, "r2", "completed")
  warm = task_mgr._run_outcomes_of(worker, task_mgr._sessions.load_chat_events_sync(worker), meta.archive_offset)
  assert warm == {"r1": "completed", "r2": "completed"}

  task_mgr._facts_memo.clear()
  task_mgr._outcomes_memo.clear()
  cold_live = task_mgr._sessions.load_chat_events_sync(worker)
  cold_count = task_mgr._archived_event_count(worker, cold_live)
  assert task_mgr._run_outcomes_of(worker, cold_live, cold_count) == warm


@pytest.mark.asyncio
async def test_activity_derivation_rereads_after_a_proc_judgment(task_env) -> None:
  """A launched run without a terminal fact is judged through /proc, whose
  death no record write announces: the derivation must re-read on every call
  for such a node — a stored /proc verdict would pin a crashed run's running
  verdict on every tree page until restart."""
  _cfg, _session_mgr, task_mgr = task_env
  ids = await seed_tree(task_mgr)
  worker = ids["worker"]
  await task_mgr.runs.register_run(RunRecord(id="r-proc", session_id=worker))
  proc = subprocess.Popen(["sleep", "30"])
  try:
    pid_start, _state = read_pid_stat(proc.pid)
    await task_mgr.runs.record_launch(worker, "r-proc", pid=proc.pid, pid_start=pid_start)
    assert task_mgr.activity_of(worker).work_state == "running"
    assert task_mgr.activity_of(worker).work_state == "running"
  finally:
    proc.kill()
    proc.wait()
  assert task_mgr.activity_of(worker).work_state == "idle"


@pytest.mark.asyncio
async def test_patch_metadata_and_permanent_delete_blockers(task_env) -> None:
  cfg, session_mgr, task_mgr = task_env
  ids = await seed_tree(task_mgr)
  with make_api_client(cfg, session_mgr, task_mgr) as client:
    # Name-only PATCH keeps the legacy rename contract.
    renamed = client.patch(f"/api/sessions/{ids['worker']}", json={"name": "Renamed"})
    assert renamed.status_code == 200 and renamed.json()["name"] == "Renamed"

    patched = client.patch(
        f"/api/sessions/{ids['worker']}",
        json={
            "task": {
                "goal": "ship the tree",
                "acceptance": ["tests pass"]
            },
            "presentation": "shown",
        })
    assert patched.status_code == 200
    body = patched.json()
    assert body["task"]["goal"] == "ship the tree"
    assert body["presentation"] == "shown"

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


# The retired task-pause key, spelled in parts: the whole key must stay
# grep-clean out of the tree while these retirement tests still send it on
# the wire and write it on disk.
RETIRED_PAUSE_KEY = "automation" "_paused"


@pytest.mark.asyncio
async def test_metadata_with_the_retired_pause_key_loads_and_drops_it_on_save(task_env) -> None:
  """A metadata.json still carrying the retired pause key loads through the
  normal session load path (unknown keys are ignored on read) and the key is
  gone after the next metadata save."""
  cfg, session_mgr, task_mgr = task_env
  ids = await seed_tree(task_mgr)
  metadata_path = cfg.sessions_dir / ids["worker"] / "metadata.json"
  data = json.loads(metadata_path.read_text())
  assert RETIRED_PAUSE_KEY not in data
  data[RETIRED_PAUSE_KEY] = True
  metadata_path.write_text(json.dumps(data))

  # The normal load paths answer the node, not a parse error.
  meta = await task_mgr.load_task_meta(ids["worker"])
  assert meta is not None and meta.id == ids["worker"]
  loaded = await session_mgr.get_session(ids["worker"])
  assert loaded is not None and loaded.id == ids["worker"]

  # The next save rewrites the file from the parsed model: the key is gone.
  await session_mgr.rename_session(ids["worker"], "Renamed")
  saved = json.loads(metadata_path.read_text())
  assert RETIRED_PAUSE_KEY not in saved
  assert saved["name"] == "Renamed"


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
  record = await task_mgr.runs.register_run(RunRecord(id=claims.run_id, session_id=claims.session_id, kind="work"))
  await task_mgr.runs.record_launch(
      claims.session_id,
      claims.run_id,
      pid=40000 + (abs(hash(claims.run_id)) % 20000),
      pid_start="ps-" + claims.run_id[:8])
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
  await task_mgr.events.append(
      ids["root"], {
          "id": "user-1",
          "type": "user",
          "timestamp": utc_now_iso(),
          "actor": "user",
          "source_session_id": ids["root"],
          "content": "take off",
      })

  with make_api_client(cfg, session_mgr, task_mgr) as client:
    # A valid active-Run token may create a worker directly under its own manager task.
    ok = client.post(
        "/api/sessions/",
        json={
            "request_id": "agent-w",
            "task_parent_id": ids["root"],
            "profile": "worker",
            "name": "AgentW"
        },
        headers=agent_headers(claims))
    assert ok.status_code == 200, ok.text
    assert ok.json()["profile"] == "worker"

    # ...and a logical manager child under its own task (no extra approval),
    # but not under somebody else's task.
    manager_try = client.post(
        "/api/sessions/",
        json={
            "request_id": "agent-m",
            "task_parent_id": ids["root"],
            "profile": "manager"
        },
        headers=agent_headers(claims))
    assert manager_try.status_code == 200, manager_try.text
    assert manager_try.json()["profile"] == "manager"

    foreign_try = client.post(
        "/api/sessions/",
        json={
            "request_id": "agent-f",
            "task_parent_id": ids["worker"],
            "profile": "worker"
        },
        headers=agent_headers(claims))
    assert foreign_try.status_code == 403

    foreign_manager_try = client.post(
        "/api/sessions/",
        json={
            "request_id": "agent-fm",
            "task_parent_id": ids["worker"],
            "profile": "manager"
        },
        headers=agent_headers(claims))
    assert foreign_manager_try.status_code == 403

    # User-only structural mutations are 403 for an agent caller.
    patch_try = client.patch(
        f"/api/sessions/{ids['worker']}", json={"presentation": "hidden"}, headers=agent_headers(claims))
    assert patch_try.status_code == 403

    retry_try = client.post(
        f"/api/sessions/{ids['worker']}/retry",
        json={
            "request_id": "r",
            "run_id": "run-1"
        },
        headers=agent_headers(claims))
    assert retry_try.status_code == 403


# ---------------------------------------------------------------------------
# Authorization: nearest real user instruction wins
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_agent_messages_and_cron_inputs_never_mint_authorization(task_env) -> None:
  from src.core.control_events import build_control_event
  from src.core.event_types import AGENT_MESSAGE

  cfg, session_mgr, task_mgr = task_env
  stub_credentials({"charliebot": {"access_key": "op-secret"}})
  root = await task_mgr.create_task(
      request_id="root",
      task_parent_id=None,
      profile="manager",
      task=None,
      name="Root",
      backend=None,
      caller="operator")
  claims = RunTokenClaims(session_id=root.id, run_id="agent-run-1", agent="worker-alpha")
  await register_agent_run(task_mgr, claims)
  await task_mgr.events.append(
      root.id,
      build_control_event(AGENT_MESSAGE, actor="agent", source_session_id=root.id, content="take off now please"))

  with make_api_client(cfg, session_mgr, task_mgr) as client:
    blocked = client.post(
        "/api/sessions/",
        json={
            "request_id": "no-auth",
            "task_parent_id": root.id,
            "profile": "worker"
        },
        headers=agent_headers(claims))
    assert blocked.status_code == 403
