"""Visible running state and the worker transcript projection.

The task-tree rollout left a running worker without a header timer and its
record reachable only through the old card panel. The sidebar's running state
is the task-tree activity derivation's; what this
file pins is the worker node's busy interval — thinking_state opens it at the
Run's recorded started_at and the Run's own terminal fact closes it — and the
display-backend map. The worker node's messages are its Runs' events projected
through the ordinary message aggregator, so the main chat view renders them
with pagination and the delivery close.
"""

from __future__ import annotations

import datetime
import json
import pathlib

import conftest
import pytest

from src.infra import event_types as ET
from src.infra import models
from src.runtime import task_sessions, thinking_state
from tests import test_task_execution


async def manager_with_worker(tmp_path, monkeypatch):
  """One root manager with one worker child, no user input anywhere."""
  cfg, session_blocks, tree = test_task_execution.build_env(tmp_path, monkeypatch)
  conftest.stub_credentials({"charliebot": {"access_key": "vis-key"}})
  root = await tree.create_task(
      request_id="root",
      task_parent_id=None,
      profile="manager",
      task=models.TaskSpec(goal="project"),
      name="Project",
      backend=None,
      caller=conftest.OPERATOR)
  worker = await tree.create_task(
      request_id="w",
      task_parent_id=root.id,
      profile="worker",
      task=models.TaskSpec(goal="leaf"),
      name="W",
      backend=None,
      caller=conftest.OPERATOR)
  return cfg, session_blocks, tree, root, worker


@pytest.mark.asyncio
async def test_worker_run_opens_the_busy_interval_and_every_finish_closes_it(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A worker Run's launch opens its node's busy interval at the recorded
  started_at (the header timer) and records its display backend; each
  terminal outcome closes exactly that interval. The manager parent's own
  master-queue interval is never borrowed."""
  _cfg, _session_blocks, tree, root, worker = await manager_with_worker(tmp_path, monkeypatch)
  started = datetime.datetime.now(datetime.UTC) - datetime.timedelta(seconds=3)
  await tree.runs.register_run(
      models.RunRecord(
          id="run-1", session_id=worker.id, kind="work", backend="fake", model="fake-model", started_at=started))
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
async def test_a_run_closes_only_the_interval_it_opened(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Manager turns leave their queue interval open; child runs close only their
  own interval, and queued runs close nothing on finish."""
  _cfg, _session_blocks, tree, root, worker = await manager_with_worker(tmp_path, monkeypatch)
  queue_since = datetime.datetime.now(datetime.UTC) - datetime.timedelta(seconds=9)
  thinking_state.mark_busy(root.id, since=queue_since)
  await tree.runs.register_run(
      models.RunRecord(
          id="turn-1", session_id=root.id, kind="manager_turn", started_at=datetime.datetime.now(datetime.UTC)))
  await tree.runs.record_launch(root.id, "turn-1", pid=424250, pid_start="1-424010")
  await tree.runs.record_finish(root.id, "turn-1", "success")
  assert thinking_state.busy_since(root.id) == queue_since

  child = worker
  await tree.runs.register_run(
      models.RunRecord(
          id="run-l",
          session_id=child.id,
          kind="work",
          backend="fake",
          model="fake-model",
          started_at=datetime.datetime.now(datetime.UTC)))
  await tree.runs.record_launch(child.id, "run-l", pid=424244, pid_start="1-424002")
  assert thinking_state.busy_since(child.id) is not None
  await tree.runs.record_finish(child.id, "run-l", "success")
  assert thinking_state.busy_since(child.id) is None
  assert thinking_state.busy_since(root.id) == queue_since

  worker_since = datetime.datetime.now(datetime.UTC) - datetime.timedelta(seconds=2)
  await tree.runs.register_run(
      models.RunRecord(
          id="run-live", session_id=worker.id, kind="work", backend="fake", model="fake-model",
          started_at=worker_since))
  await tree.runs.record_launch(worker.id, "run-live", pid=424251, pid_start="1-424011")
  await tree.runs.register_run(
      models.RunRecord(id="run-queued", session_id=worker.id, kind="work", backend="fake", model="fake-model"))
  await tree.runs.record_finish(worker.id, "run-queued", "cancelled")
  assert thinking_state.busy_since(worker.id) == worker_since


# ---------------------------------------------------------------------------
# The worker transcript projection
# ---------------------------------------------------------------------------


def _write_run_events(tree: task_sessions.TaskTreeManager, session_id: str, run_id: str, events: list[dict]) -> None:
  path = tree.runs.run_dir(session_id, run_id) / "events.jsonl"
  path.parent.mkdir(parents=True, exist_ok=True)
  with path.open("a", encoding="utf-8") as f:
    for event in events:
      f.write(json.dumps(event) + "\n")


@pytest.mark.asyncio
async def test_failed_run_header_reads_failed_with_its_error(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A Run that failed before its process started (no started_at — the launch
  refusal precedes any spawn, and a registered Run carries none) heads its
  segment as "launch failed" with the error text in full, exactly once: the
  chosen error event's own chat row leaves the projection."""
  from src.runtime import worker_transcript

  _cfg, _session_blocks, tree, _root, worker = await manager_with_worker(tmp_path, monkeypatch)
  error_text = "RuntimeError: worktree preparation failed: task/x differs from origin/main"
  # Registered without a task spec (the improve-iteration shape): no task_spec_ref.
  await tree.runs.register_run(
      models.RunRecord(id="run-f", session_id=worker.id, kind="work", backend="fake", model="fake-model"))
  _write_run_events(
      tree, worker.id, "run-f", [
          {
              "type": ET.ERROR,
              "message": error_text,
              "content": error_text,
              "timestamp": models.utc_now_iso()
          },
      ])
  await tree.dispatch.finish_run(worker.id, "run-f", outcome="failed", exit_code=-1)

  entry = worker_transcript.load_worker_transcript(tree, worker.id)
  messages = entry.projection.committed
  header = messages[0]
  assert header["state"] == "failed"  # the state every existing reader keys off
  assert header["content"].endswith("launch failed")
  assert header["launched"] is False and header["launch_failed"] is True
  assert header["error"] == error_text
  # The error text appears exactly once — the header's error field; the chosen
  # event's own row left this projection.
  assert [m for m in messages if error_text in str(m.get("content") or "")] == []
  assert [m for m in messages if m.get("error") == error_text] == [header]
  # The drop lives in the projection only: the Run's events log keeps the line.
  raw_log = tree.runs.run_dir(worker.id, "run-f") / "events.jsonl"
  assert error_text in raw_log.read_text(encoding="utf-8")
  assert header["task_spec_ref"] == ""
  assert header["launch_prompt_ref"] == ""  # never assembled: no process started


@pytest.mark.asyncio
async def test_started_run_that_fails_reads_failed_and_links_its_launch_prompt(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A Run that started (its launch recorded) and then failed still reads plain
  "failed" — launch_failed is false — and its launch prompt link points at the
  assembled file. Its error text also lands in the header exactly once."""
  from src.runtime import task_prompts, worker_transcript

  _cfg, _session_blocks, tree, _root, worker = await manager_with_worker(tmp_path, monkeypatch)
  error_text = "RuntimeError: backend transport died mid-run"
  await tree.runs.register_run(
      models.RunRecord(id="run-s", session_id=worker.id, kind="work", backend="fake", model="fake-model"))
  # The real launch seam sets started_at (launched) and the adapter persists
  # the launch text under the run dir before spawn.
  await tree.runs.record_launch(worker.id, "run-s", pid=424401, pid_start="1-424401")
  launch_prompt = tree.runs.run_dir(worker.id, "run-s") / task_prompts.LAUNCH_TEXT_FILENAME
  launch_prompt.parent.mkdir(parents=True, exist_ok=True)
  launch_prompt.write_text("the exact launch text\n", encoding="utf-8")
  _write_run_events(
      tree, worker.id, "run-s", [
          {
              "type": ET.ERROR,
              "message": error_text,
              "content": error_text,
              "timestamp": models.utc_now_iso()
          },
      ])
  await tree.runs.record_finish(worker.id, "run-s", "failed")

  entry = worker_transcript.load_worker_transcript(tree, worker.id)
  messages = entry.projection.committed
  header = messages[0]
  assert header["state"] == "failed"
  assert header["content"].endswith("failed") and not header["content"].endswith("launch failed")
  assert header["launched"] is True and header["launch_failed"] is False
  assert header["error"] == error_text
  assert [m for m in messages if error_text in str(m.get("content") or "")] == []
  assert header["launch_prompt_ref"] == str(launch_prompt)
  assert pathlib.Path(header["launch_prompt_ref"]).is_file()


@pytest.mark.asyncio
async def test_transcript_poll_moves_a_failed_run_s_error_into_its_header(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A running Run's error event is an ordinary row; once the Run finishes
  failed, a poll carrying the previous revision answers reset (the client
  re-renders) and the error text lives only in the header's error field."""
  import src.runtime.runs as runs_mod

  cfg, session_blocks, tree, _root, worker = await manager_with_worker(tmp_path, monkeypatch)
  error_text = "RuntimeError: backend transport died mid-run"
  await tree.runs.register_run(
      models.RunRecord(id="run-live", session_id=worker.id, kind="work", backend="fake", model="fake-model"))
  await tree.runs.record_launch(worker.id, "run-live", pid=424451, pid_start="1-424451")
  monkeypatch.setattr(runs_mod, "is_run_alive", lambda *a, **k: True)
  _write_run_events(
      tree, worker.id, "run-live", [
          {
              "type": ET.ERROR,
              "message": error_text,
              "content": error_text,
              "timestamp": models.utc_now_iso()
          },
      ])

  with test_task_execution.make_api_client(cfg, session_blocks, tree) as client:
    running = client.get(f"/api/sessions/{worker.id}/transcript?after=0&revision=")
    assert running.status_code == 200, running.text
    running_body = running.json()
    assert running_body["reset"] is True
    running_header = next(m for m in running_body["messages"] if m.get("kind") == ET.RUN_HEADER)
    assert running_header["state"] == "running" and running_header["error"] == ""
    # The error event is an ordinary row while the Run runs.
    error_rows = [m for m in running_body["messages"] if error_text in str(m.get("content") or "")]
    assert len(error_rows) == 1 and error_rows[0]["role"] == "system"

    await tree.runs.record_finish(worker.id, "run-live", "failed")

    failed = client.get(
        f"/api/sessions/{worker.id}/transcript?after={running_body['total']}&revision={running_body['revision']}")
    assert failed.status_code == 200, failed.text
    failed_body = failed.json()
    assert failed_body["reset"] is True
    failed_header = next(m for m in failed_body["messages"] if m.get("kind") == ET.RUN_HEADER)
    assert failed_header["state"] == "failed" and failed_header["error"] == error_text
    assert failed_header["launched"] is True and failed_header["launch_failed"] is False
    # The error text appears exactly once: the header's error field.
    assert [m for m in failed_body["messages"] if error_text in str(m.get("content") or "")] == []
    assert [m for m in failed_body["messages"] if m.get("error") == error_text] == [failed_header]
