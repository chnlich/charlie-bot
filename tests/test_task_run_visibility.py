"""Visible running state and the worker transcript projection.

The task-tree rollout left a running worker without a header timer and its
record reachable only through the old card panel. The sidebar's running state
is the task-tree activity derivation's (test_task_tree_activity.py); what this
file pins is the worker node's busy interval — thinking_state opens it at the
Run's recorded started_at and the Run's own terminal fact closes it — and the
display-backend map. The worker node's messages are its Runs' events projected
through the ordinary message aggregator, so the main chat view renders them
with pagination and the delivery close.
"""

from __future__ import annotations

import asyncio
import json
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest
from conftest import patch_instructions_content, stub_credentials

from src.core import event_types as ET
from src.core import thinking_state
from src.core.control_events import build_control_event
from src.core.models import CreateSessionRequest, RunRecord, TaskSpec
from src.core.run_token import CallerIdentity
from src.core.task_sessions import TaskTreeManager
from tests.test_task_execution import (
  _adapter_with_silent_broadcast,
  build_env,
  make_api_client,
  wait_for_terminal_run,
)

OP_CALLER = CallerIdentity(kind="operator")


async def manager_with_worker(tmp_path, monkeypatch):
  """One root manager with one worker child, no user input anywhere."""
  cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
  patch_instructions_content(monkeypatch)
  stub_credentials({"charliebot": {"access_key": "vis-key"}})
  root = await tree.create_task(
      request_id="root", task_parent_id=None, profile="manager",
      task=TaskSpec(goal="project"), name="Project", backend=None, caller=OP_CALLER)
  worker = await tree.create_task(
      request_id="w", task_parent_id=root.id, profile="worker",
      task=TaskSpec(goal="leaf"), name="W", backend=None, caller=OP_CALLER)
  return cfg, session_mgr, tree, root, worker


@pytest.mark.asyncio
async def test_worker_run_opens_the_busy_interval_and_every_finish_closes_it(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A worker Run's launch opens its node's busy interval at the recorded
  started_at (the header timer) and records its display backend; each
  terminal outcome closes exactly that interval. The manager parent's own
  master-queue interval is never borrowed."""
  _cfg, _session_mgr, tree, root, worker = await manager_with_worker(tmp_path, monkeypatch)
  started = datetime.now(UTC) - timedelta(seconds=3)
  await tree.runs.register_run(RunRecord(
      id="run-1", session_id=worker.id, kind="work",
      backend="fake", model="fake-model", started_at=started))
  await tree.runs.record_launch(worker.id, "run-1", pid=424242, pid_start="1-424000")

  assert thinking_state.busy_since(worker.id) == started
  assert thinking_state.run_backend(worker.id) == "fake"
  assert thinking_state.busy_since(root.id) is None

  for outcome in ("success", "failed", "cancelled"):
    # Each outcome rides the one terminal funnel (record_finish writes the
    # durable fact; record_launch re-opens the interval for the next scenario).
    await tree.runs.record_finish(worker.id, "run-1", outcome)
    assert thinking_state.busy_since(worker.id) is None, outcome
    if outcome != "cancelled":
      await tree.runs.record_launch(worker.id, "run-1", pid=424242, pid_start="1-424000")


@pytest.mark.asyncio
async def test_dispatch_finish_funnel_closes_the_same_interval(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The adapter's finish path (dispatch.finish_run) lands the same clearance —
  the funnel a cancelled, crashed or launch-failed Run's follow drives."""
  _cfg, _session_mgr, tree, _root, worker = await manager_with_worker(tmp_path, monkeypatch)
  await tree.runs.register_run(RunRecord(
      id="run-d", session_id=worker.id, kind="work",
      backend="fake", model="fake-model", started_at=datetime.now(UTC)))
  await tree.runs.record_launch(worker.id, "run-d", pid=424243, pid_start="1-424001")
  assert thinking_state.busy_since(worker.id) is not None

  await tree.dispatch.finish_run(worker.id, "run-d", outcome="failed")
  assert thinking_state.busy_since(worker.id) is None


@pytest.mark.asyncio
async def test_a_run_closes_only_the_interval_it_opened(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A manager_turn Run opens no interval, so its finish leaves the master
  queue's own interval standing; a legacy parent's interval is untouched by a
  child Run; a queued Run that never launched closes nothing on finish."""
  _cfg, session_mgr, tree, root, worker = await manager_with_worker(tmp_path, monkeypatch)
  queue_since = datetime.now(UTC) - timedelta(seconds=9)
  thinking_state.mark_busy(root.id, since=queue_since)
  await tree.runs.register_run(RunRecord(
      id="turn-1", session_id=root.id, kind="manager_turn", started_at=datetime.now(UTC)))
  await tree.runs.record_launch(root.id, "turn-1", pid=424250, pid_start="1-424010")
  await tree.runs.record_finish(root.id, "turn-1", "success")
  assert thinking_state.busy_since(root.id) == queue_since

  legacy = await session_mgr.create_session(CreateSessionRequest(name="Legacy"), backend="fake")
  legacy_since = datetime.now(UTC) - timedelta(seconds=5)
  thinking_state.mark_busy(legacy.id, since=legacy_since)
  child = await tree.create_task(
      request_id="lw", task_parent_id=legacy.id, profile="worker",
      task=TaskSpec(goal="leaf"), name="LW", backend=None, caller=OP_CALLER)
  await tree.runs.register_run(RunRecord(
      id="run-l", session_id=child.id, kind="work",
      backend="fake", model="fake-model", started_at=datetime.now(UTC)))
  await tree.runs.record_launch(child.id, "run-l", pid=424244, pid_start="1-424002")
  assert thinking_state.busy_since(child.id) is not None
  await tree.runs.record_finish(child.id, "run-l", "success")
  assert thinking_state.busy_since(child.id) is None
  assert thinking_state.busy_since(legacy.id) == legacy_since

  worker_since = datetime.now(UTC) - timedelta(seconds=2)
  await tree.runs.register_run(RunRecord(
      id="run-live", session_id=worker.id, kind="work",
      backend="fake", model="fake-model", started_at=worker_since))
  await tree.runs.record_launch(worker.id, "run-live", pid=424251, pid_start="1-424011")
  await tree.runs.register_run(RunRecord(
      id="run-queued", session_id=worker.id, kind="work", backend="fake", model="fake-model"))
  await tree.runs.record_finish(worker.id, "run-queued", "cancelled")
  assert thinking_state.busy_since(worker.id) == worker_since


@pytest.mark.asyncio
async def test_resume_remarks_busy_at_the_recorded_started_at(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A server restart drops the in-memory busy interval; the recovery pass's
  re-attach of a live Run re-opens it at the Run's recorded started_at, and
  the follow's terminal fact closes it again."""
  from src.core.task_recovery import reconcile_task_tree
  from tests.test_task_recovery import install_resume_ready_backends

  cfg, session_mgr, tree, _root, worker = await manager_with_worker(tmp_path, monkeypatch)
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
  install_resume_ready_backends(monkeypatch, [])
  stub_credentials({"charliebot": {"access_key": "vis-key"}})

  started = datetime.now(UTC) - timedelta(seconds=1)
  await tree.runs.register_run(RunRecord(
      id="run-r", session_id=worker.id, kind="work",
      backend="fake", model="fake-model", started_at=started))
  await tree.runs.record_launch(worker.id, "run-r", pid=424777, pid_start="1-424000")

  gate = asyncio.Event()
  import src.core.runs as runs_mod
  monkeypatch.setattr(runs_mod, "is_run_alive", lambda *a, **k: not gate.is_set())

  # The restart: every in-memory running-state fact is gone.
  thinking_state.reset_run_state_for_tests()
  assert thinking_state.busy_since(worker.id) is None

  reconcile = asyncio.get_running_loop().create_task(
      reconcile_task_tree(cfg, tree, session_mgr))
  for _ in range(250):
    if thinking_state.busy_since(worker.id) is not None:
      break
    await asyncio.sleep(0.02)
  assert thinking_state.busy_since(worker.id) == started

  gate.set()
  await wait_for_terminal_run(tree, worker.id, "run-r")
  await reconcile
  assert thinking_state.busy_since(worker.id) is None


# ---------------------------------------------------------------------------
# The worker transcript projection
# ---------------------------------------------------------------------------

def _write_run_events(tree: TaskTreeManager, session_id: str, run_id: str, events: list[dict]) -> None:
  path = tree.runs.run_dir(session_id, run_id) / "events.jsonl"
  path.parent.mkdir(parents=True, exist_ok=True)
  with path.open("a", encoding="utf-8") as f:
    for event in events:
      f.write(json.dumps(event) + "\n")


def _assistant_event(content: str, timestamp: str) -> dict:
  """One backend assistant event in the CC block shape the aggregator reads."""
  return {
      "type": ET.ASSISTANT,
      "message": {"content": [{"type": "text", "text": content}]},
      "timestamp": timestamp,
  }


async def _worker_with_two_runs(tree: TaskTreeManager, worker_id: str):
  """One delivered work Run and one queued review Run, in Run order."""
  t1 = datetime.now(UTC) - timedelta(minutes=5)
  t1_done = t1 + timedelta(seconds=30)
  t2 = datetime.now(UTC)
  await tree.runs.register_run(RunRecord(
      id="run-1", session_id=worker_id, kind="work",
      backend="fake", model="fake-model", started_at=t1, ended_at=t1_done,
      raw_log_ref="/h/runs/run-1/raw.log", events_ref="/h/runs/run-1/events.jsonl",
      result_ref="/h/runs/run-1/raw.log", repo_path="/repo", base_branch="main",
      branch_name="task/run-1"))
  _write_run_events(tree, worker_id, "run-1", [
      {"type": ET.USER, "content": "fix the parser", "timestamp": t1.isoformat()},
      _assistant_event("done", t1_done.isoformat()),
      {"type": ET.MASTER_DONE, "timestamp": t1_done.isoformat()},
  ])
  await tree.runs.record_launch(worker_id, "run-1", pid=424001, pid_start="1-424001")
  await tree.runs.record_finish(worker_id, "run-1", "success")

  # The review Run is registered (queued): a signallable state without a live
  # process, so the projection needs no liveness fakery.
  await tree.runs.register_run(RunRecord(
      id="run-2", session_id=worker_id, kind="review",
      backend="fake", model="fake-model", started_at=t2))
  _write_run_events(tree, worker_id, "run-2", [
      _assistant_event("reviewing", t2.isoformat()),
  ])

  close_event = build_control_event(
      ET.TASK_CLOSED, actor="worker", source_session_id=worker_id,
      request_id="req-1", outcome="completed", summary="shipped the parser",
      result_refs=["evidence/a.txt"], run_ids=["run-1"])
  await tree.events.append(worker_id, close_event)


@pytest.mark.asyncio
async def test_worker_transcript_headers_delivery_and_pagination(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Two Runs project as two header lines around their events, the delivery
  close lands last with the four evidence links, and the projection's tail and
  slice_before page the same cursor space the /events route serves."""
  from src.core import worker_transcript

  _cfg, _session_mgr, tree, _root, worker = await manager_with_worker(tmp_path, monkeypatch)
  await _worker_with_two_runs(tree, worker.id)

  entry = worker_transcript.load_worker_transcript(tree, worker.id)
  messages = entry.projection.committed
  roles = [m["role"] for m in messages]
  # run-1's header, its user turn (assistant + separator), run-2's header, the
  # review turn the delivery close flushes, and the delivery close itself.
  assert roles == ["system", "user", "assistant", "separator", "system",
                   "assistant", "run_delivery"]
  header1, header2, delivery = messages[0], messages[4], messages[-1]
  assert "work" in header1["content"] and "Fake" in header1["content"]
  assert header1["content"].endswith("success")
  assert "review" in header2["content"] and header2["content"].endswith("queued")
  assert delivery["task_state"] == "completed" and delivery["completed"] is True
  assert delivery["content"] == "shipped the parser"
  assert delivery["raw_log_ref"] == "/h/runs/run-1/raw.log"
  assert delivery["events_ref"] == "/h/runs/run-1/events.jsonl"
  assert delivery["result_ref"] == "/h/runs/run-1/raw.log"
  assert delivery["branch_name"] == "task/run-1" and delivery["repo_path"] == "/repo"
  # The header lines carry the Run's own state for the chat's state dot.
  assert header1["kind"] == ET.RUN_HEADER and header1["run_id"] == "run-1"
  assert header1["state"] == "success" and header1["error"] == ""
  assert header2["run_id"] == "run-2" and header2["state"] == "queued"
  # The signallable run is the queued review Run.
  assert entry.active_run_id == "run-2"

  # One tail page and one older page over the same ordinals. Pages are
  # turn-aligned: the tail's raw start lands after the MASTER_DONE separator,
  # so the page holds exactly the review Run's segment plus the close.
  tail, oldest, has_more = entry.projection.tail(3)
  assert [m["role"] for m in tail] == roles[4:]
  assert has_more and oldest == 4
  older, next_before, more = entry.projection.slice_before(oldest, 10)
  assert [m["role"] for m in older] == roles[:4]
  assert more is False and next_before == 0


@pytest.mark.asyncio
async def test_failed_run_header_reads_failed_with_its_error(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A Run that failed before its process started (its events log holds only
  the launch error) heads its segment as failed with the error text in full;
  the error event itself still renders in the segment."""
  from src.core import worker_transcript

  _cfg, _session_mgr, tree, _root, worker = await manager_with_worker(tmp_path, monkeypatch)
  error_text = "RuntimeError: worktree preparation failed: task/x differs from origin/main"
  await tree.runs.register_run(RunRecord(
      id="run-f", session_id=worker.id, kind="work",
      backend="fake", model="fake-model", started_at=datetime.now(UTC)))
  _write_run_events(tree, worker.id, "run-f", [
      {"type": ET.ERROR, "message": error_text, "content": error_text,
       "timestamp": datetime.now(UTC).isoformat()},
  ])
  await tree.dispatch.finish_run(worker.id, "run-f", outcome="failed", exit_code=-1)

  entry = worker_transcript.load_worker_transcript(tree, worker.id)
  header, error_line = entry.projection.committed[:2]
  assert header["state"] == "failed" and header["content"].endswith("failed")
  assert header["error"] == error_text
  assert error_line["role"] == "system" and error_text in error_line["content"]


@pytest.mark.asyncio
async def test_worker_transcript_api_bootstrap_transcript_and_events(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The read paths a worker page consumes — bootstrap, /transcript poll,
  /events pagination, /usage — serve the projection, and an unchanged
  transcript poll answers reset-free with the same revision."""
  cfg, session_mgr, tree, root, worker = await manager_with_worker(tmp_path, monkeypatch)
  await _worker_with_two_runs(tree, worker.id)

  with make_api_client(cfg, session_mgr, tree) as client:
    bootstrap = client.get(f"/api/sessions/{worker.id}/bootstrap")
    assert bootstrap.status_code == 200, bootstrap.text
    body = bootstrap.json()
    roles = [m["role"] for m in body["messages"]]
    # The header rides the system-pill role; the delivery close is its own role.
    assert roles[0] == "system" and roles[-1] == "run_delivery"

    first = client.get(f"/api/sessions/{worker.id}/transcript?after=0&revision=")
    assert first.status_code == 200, first.text
    data = first.json()
    assert data["reset"] is True
    assert [m["role"] for m in data["messages"]] == roles
    assert data["active_run_id"] == "run-2"

    steady = client.get(
        f"/api/sessions/{worker.id}/transcript?after={data['total']}&revision={data['revision']}")
    steady_body = steady.json()
    assert steady_body["reset"] is False and steady_body["messages"] == []

    # The /events pages are turn-aligned over the transcript's own ordinals:
    # a before below the MASTER_DONE separator snaps to the transcript start.
    page = client.get(f"/api/sessions/{worker.id}/events?before={data['total']}&limit=3")
    assert page.status_code == 200, page.text
    page_body = page.json()
    assert [m["role"] for m in page_body["messages"]] == ["system", "assistant", "run_delivery"]
    assert page_body["has_more"] is True and page_body["next_before"] == 4
    older = client.get(f"/api/sessions/{worker.id}/events?before=4&limit=10")
    older_body = older.json()
    assert [m["role"] for m in older_body["messages"]] ==         ["system", "user", "assistant", "separator"]
    assert older_body["has_more"] is False and older_body["next_before"] == 0

    usage = client.get(f"/api/sessions/{worker.id}/usage")
    assert usage.status_code == 200, usage.text
    assert usage.json()["active_run_id"] == "run-2"

  # A non-worker session has no worker transcript to serve.
  with make_api_client(cfg, session_mgr, tree) as client:
    refused = client.get(f"/api/sessions/{root.id}/transcript?after=0&revision=")
    assert refused.status_code == 400


def test_legacy_thread_transcript_and_running_timer(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A legacy worker thread projects through the same shape, addressed by the
  parent session: one header line, its events, and the running timer anchor."""
  from src.core import worker_transcript
  from src.core.models import ThreadMetadata
  from src.core.threads import thread_events_log_path

  cfg, _session_mgr, _tree = build_env(tmp_path, None)
  session_dir = tmp_path / "legacy-session"
  thread_id = "thread-legacy-1"
  events_path = thread_events_log_path(session_dir, thread_id)
  events_path.parent.mkdir(parents=True, exist_ok=True)
  started = datetime.now(UTC)
  events_path.write_text(json.dumps(
      {"type": ET.USER, "content": "old delegation", "timestamp": started.isoformat()}) + "\n",
      encoding="utf-8")
  meta = ThreadMetadata(
      id=thread_id, session_id="legacy-session", description="Review: ## Goal",
      status="running", started_at=started, backend="fake",
      pid=424999, pid_start="1-424999")

  import src.core.runs as runs_mod
  monkeypatch.setattr(runs_mod, "is_run_alive", lambda *a, **k: True)
  entry = worker_transcript.load_thread_transcript(
      cfg=cfg, session_dir=session_dir, meta=meta, events_path=events_path)
  roles = [m["role"] for m in entry.projection.committed]
  assert roles == ["system", "user"]
  header = entry.projection.committed[0]
  assert "thread" in header["content"] and header["content"].endswith("running")
  assert entry.active_run_id == thread_id
  assert worker_transcript.thread_thinking_since(meta) == started

  meta.status = "completed"
  assert worker_transcript.thread_thinking_since(meta) is None
