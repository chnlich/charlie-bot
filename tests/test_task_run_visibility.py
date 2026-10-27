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
from src.runtime import (
    control_events,
    message_aggregator,
    task_completion,
    task_sessions,
    thinking_state,
    worker_transcript,
)
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
  await tree.runs.register_run_locked(
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
  await tree.runs.register_run_locked(
      models.RunRecord(
          id="turn-1", session_id=root.id, kind="manager_turn", started_at=datetime.datetime.now(datetime.UTC)))
  await tree.runs.record_launch(root.id, "turn-1", pid=424250, pid_start="1-424010")
  await tree.runs.record_finish(root.id, "turn-1", "success")
  assert thinking_state.busy_since(root.id) == queue_since

  child = worker
  await tree.runs.register_run_locked(
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
  await tree.runs.register_run_locked(
      models.RunRecord(
          id="run-live", session_id=worker.id, kind="work", backend="fake", model="fake-model",
          started_at=worker_since))
  await tree.runs.record_launch(worker.id, "run-live", pid=424251, pid_start="1-424011")
  await tree.runs.register_run_locked(
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
  _cfg, _session_blocks, tree, _root, worker = await manager_with_worker(tmp_path, monkeypatch)
  error_text = "RuntimeError: worktree preparation failed: task/x differs from origin/main"
  # Registered without a task spec (the improve-iteration shape): no task_spec_ref.
  await tree.runs.register_run_locked(
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
  # A terminated Run's header line ends with its tool-call count (here zero:
  # the launch refusal wrote no tool rows).
  assert header["content"].endswith("launch failed · 0 tool calls")
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
  from src.runtime import task_prompts

  _cfg, _session_blocks, tree, _root, worker = await manager_with_worker(tmp_path, monkeypatch)
  error_text = "RuntimeError: backend transport died mid-run"
  await tree.runs.register_run_locked(
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
  assert header["content"].endswith(
      "failed · 0 tool calls") and not header["content"].endswith("launch failed · 0 tool calls")
  assert header["launched"] is True and header["launch_failed"] is False
  assert header["error"] == error_text
  assert [m for m in messages if error_text in str(m.get("content") or "")] == []
  assert header["launch_prompt_ref"] == str(launch_prompt)
  assert pathlib.Path(header["launch_prompt_ref"]).is_file()


def _last_close_timestamp(tree: task_sessions.TaskTreeManager, session_id: str) -> str:
  """The node's newest task_closed fact timestamp (a resolved task always has one)."""
  closes = [event for event in tree.fact_history(session_id) if event.get("type") == ET.TASK_CLOSED]
  assert closes
  return str(closes[-1]["timestamp"])


def _delivery_message(entry: worker_transcript.TranscriptEntry) -> dict:
  deliveries = [m for m in entry.projection.committed if m.get("role") == ET.RUN_DELIVERY]
  assert len(deliveries) == 1
  return deliveries[0]


@pytest.mark.asyncio
async def test_delivery_card_timestamp_is_the_task_close_time(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The closing delivery card carries the resolved task's task_closed fact time,
  not the projection build's clock: it reads the same across a fresh load and a
  cache clear, and a reopen/close moves it to the second close."""
  _cfg, _session_blocks, tree, _root, worker = await manager_with_worker(tmp_path, monkeypatch)
  await tree.runs.register_run_locked(
      models.RunRecord(
          id="run-1",
          session_id=worker.id,
          kind="work",
          backend="fake",
          model="fake-model",
          started_at=models.utc_now()))
  await tree.runs.record_launch(worker.id, "run-1", pid=424301, pid_start="1-424301")
  await tree.runs.record_finish(worker.id, "run-1", "success")

  await tree.completion.archive_subtree(
      worker.id,
      request_id="archive-test",
      summary="archived by the user",
      result_refs=[],
      run_ids=[],
      caller=conftest.OPERATOR)
  first_close_at = _last_close_timestamp(tree, worker.id)
  assert _delivery_message(worker_transcript.load_worker_transcript(tree, worker.id))["timestamp"] == first_close_at

  worker_transcript._worker_memo.clear()
  assert _delivery_message(worker_transcript.load_worker_transcript(tree, worker.id))["timestamp"] == first_close_at

  await tree.completion.restore_task(worker.id, request_id="reopen-1", reason="test", caller=conftest.OPERATOR)
  await tree.completion.archive_subtree(
      worker.id,
      request_id="archive-test",
      summary="archived by the user",
      result_refs=[],
      run_ids=[],
      caller=conftest.OPERATOR)
  second_close_at = _last_close_timestamp(tree, worker.id)
  assert second_close_at != first_close_at

  worker_transcript._worker_memo.clear()
  assert _delivery_message(worker_transcript.load_worker_transcript(tree, worker.id))["timestamp"] == second_close_at


@pytest.mark.asyncio
async def test_summary_and_delivery_read_only_the_current_close(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A reopen clears the close the summary and the delivery card read; a later
  close with no refs carries no landed commit from the earlier one."""
  _cfg, _session_blocks, tree, _root, worker = await manager_with_worker(tmp_path, monkeypatch)
  await tree.runs.register_run_locked(
      models.RunRecord(
          id="run-1",
          session_id=worker.id,
          kind="work",
          backend="fake",
          model="fake-model",
          started_at=models.utc_now()))
  await tree.runs.record_launch(worker.id, "run-1", pid=424302, pid_start="1-424302")
  await tree.runs.record_finish(worker.id, "run-1", "success")

  async def close(request_id: str, outcome: str, result_refs: list[str]) -> None:
    meta = await tree.load_meta(worker.id)
    assert meta is not None
    async with tree.control_lock:
      await tree.completion._append_closed(
          worker.id,
          request_id=request_id,
          outcome=outcome,
          summary=outcome,
          result_refs=result_refs,
          run_ids=["run-1"],
          actor=control_events.ACTOR_SYSTEM,
          report_to=meta.task_parent_id,
          archived_with=None)

  await close("close-1", "completed", ["run:run-1", f"{task_completion.LANDING_REF_PREFIX}origin/main@aaa111"])
  assert worker_transcript.load_worker_transcript(tree, worker.id).summary["landed"] == "origin/main@aaa111"

  await tree.completion.restore_task(worker.id, request_id="reopen-1", reason="test", caller=conftest.OPERATOR)
  reopened = worker_transcript.load_worker_transcript(tree, worker.id)
  assert reopened.summary["task_state"] == "open"
  assert [reopened.summary["closed_at"], reopened.summary["landed"], reopened.summary["delivery"]] == [None, None, None]
  assert [m for m in reopened.projection.committed if m.get("role") == ET.RUN_DELIVERY] == []

  await close("close-2", "cancelled", [])
  second_close_at = _last_close_timestamp(tree, worker.id)
  closed = worker_transcript.load_worker_transcript(tree, worker.id)
  assert closed.summary["landed"] is None
  assert closed.summary["closed_at"] == second_close_at
  assert closed.summary["delivery"]["run_id"] == "run-1"
  delivery = _delivery_message(closed)
  assert delivery["result_refs"] == []
  assert delivery["timestamp"] == second_close_at


# ---------------------------------------------------------------------------
# The summary card's one object (plan 4.2)
# ---------------------------------------------------------------------------


def _assistant_text_event(text: str) -> dict:
  return {
      "type": ET.ASSISTANT,
      "message": {
          "content": [{
              "type": "text",
              "text": text
          }]
      },
      "timestamp": models.utc_now_iso(),
  }


@pytest.mark.asyncio
async def test_summary_follows_a_running_run_and_its_streaming_appends(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A live run: the card's active block shows its counts and the newest text
  and tool with their own timestamps; a streaming append rebuilds the memo
  (the summary follows) but does not move the client-facing revision."""
  import src.runtime.runs as runs_mod

  _cfg, _session_blocks, tree, _root, worker = await manager_with_worker(tmp_path, monkeypatch)
  await tree.runs.register_run_locked(
      models.RunRecord(
          id="run-live",
          session_id=worker.id,
          kind="work",
          backend="fake",
          model="fake-model",
          started_at=datetime.datetime.now(datetime.UTC)))
  await tree.runs.record_launch(worker.id, "run-live", pid=424461, pid_start="1-424461")
  monkeypatch.setattr(runs_mod, "is_run_alive", lambda *a, **k: True)

  entry = worker_transcript.load_worker_transcript(tree, worker.id)
  summary = entry.summary
  assert summary["task_state"] == "open" and summary["delivery"] is None
  assert summary["active"]["run_id"] == "run-live"
  assert summary["active"]["last_text"] is None and summary["active"]["last_tool"] is None
  assert summary["closing"]["work"] == {"run_id": "run-live", "text": None}
  # A live run reads no tool-call count in its header row.
  assert summary["runs"][0]["tool_calls"] == 0
  # ... and its projection carries no run_end separator while it runs.
  assert [m for m in entry.projection.committed if m.get("kind") == ET.RUN_END] == []

  revision = entry.revision
  second = worker_transcript.load_worker_transcript(tree, worker.id)
  assert second.revision == revision  # an unchanged read returns the memo

  _write_run_events(
      tree, worker.id, "run-live", [
          {
              "type": ET.TOOL_USE,
              "name": "Read",
              "input": {
                  "file_path": "/tmp/notes.md"
              },
              "timestamp": models.utc_now_iso()
          },
          _assistant_text_event("Working through it."),
          {
              "type": ET.RESULT,
              "result": "A later result event",
              "timestamp": models.utc_now_iso()
          },
      ])
  entry = worker_transcript.load_worker_transcript(tree, worker.id)
  summary = entry.summary
  assert summary["active"]["tool_calls"] == 1
  assert summary["active"]["last_tool"]["name"] == "Read"
  assert summary["active"]["last_text"] == {
      "timestamp": summary["active"]["last_text"]["timestamp"],
      "text": "Working through it."
  }
  assert summary["closing"]["work"]["text"] == "A later result event"
  assert summary["active"]["last_tool"]["timestamp"]
  assert entry.revision == revision  # a streaming append never resets the page
  assert summary["runs"][0]["tool_calls"] == 1

  await tree.runs.record_finish(worker.id, "run-live", "success")
  finished = worker_transcript.load_worker_transcript(tree, worker.id)
  assert finished.revision != revision  # a Run state change requires a full render
  finished_revision = finished.revision
  _write_run_events(tree, worker.id, "run-live", [_assistant_text_event("Post-finish event update.")])
  updated = worker_transcript.load_worker_transcript(tree, worker.id)
  assert updated.revision != finished_revision  # terminal Run log changes are part of the revision


@pytest.mark.asyncio
async def test_summary_reports_a_failed_run_cause_and_a_stop_request(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A stopped, failed run: the cause sentence rides the newest error event
  that carries a message (the newline-folded first line), the newest stop
  request fact names its run, request id and time — and the terminated run
  closes with its run_end separator."""
  _cfg, _session_blocks, tree, _root, worker = await manager_with_worker(tmp_path, monkeypatch)
  await tree.runs.register_run_locked(
      models.RunRecord(
          id="run-dead",
          session_id=worker.id,
          kind="work",
          backend="fake",
          model="fake-model",
          started_at=datetime.datetime.now(datetime.UTC)))
  await tree.runs.record_launch(worker.id, "run-dead", pid=424462, pid_start="1-424462")
  await tree.runs.request_stop(worker.id, "run-dead", request_id="chat-cancel:sess-1")
  _write_run_events(
      tree, worker.id, "run-dead", [
          {
              "type": ET.ERROR,
              "content": "peer closed connection\nwhile streaming",
              "message": "peer closed connection\nwhile streaming",
              "timestamp": models.utc_now_iso(),
          },
          {
              "type": ET.ERROR,
              "content": "newer stderr noise",
              "timestamp": models.utc_now_iso(),
          },
      ])
  ended = datetime.datetime.now(datetime.UTC)
  await tree.runs.record_finish(worker.id, "run-dead", "failed", ended_at=ended)

  entry = worker_transcript.load_worker_transcript(tree, worker.id)
  summary = entry.summary
  assert summary["task_state"] == "open"
  assert summary["cause"]["run_id"] == "run-dead"
  assert summary["cause"]["text"] == "peer closed connection"
  assert summary["cause"]["timestamp"]
  assert summary["stop"] == {
      "run_id": "run-dead",
      "request_id": "chat-cancel:sess-1",
      "timestamp": summary["stop"]["timestamp"],
  }
  messages = entry.projection.committed
  headers = [m for m in messages if m.get("kind") == ET.RUN_HEADER]
  ends = [m for m in messages if m.get("kind") == ET.RUN_END]
  assert [len(headers), len(ends)] == [1, 1]
  assert headers[0]["error"] == "peer closed connection\nwhile streaming"
  # The run_end separator closes the run at its own terminal time with its
  # duration (the two clocks both write around the finish; they may differ in
  # their microseconds).
  assert ends[0]["timestamp"] is not None
  assert ends[0]["thinking_seconds"] is not None and ends[0]["thinking_seconds"] >= 0


def test_run_cause_falls_back_to_the_newest_error_with_text() -> None:
  events = [
      {
          "type": ET.ERROR,
          "content": "usable older stderr",
          "timestamp": "older"
      },
      {
          "type": ET.ERROR,
          "content": "  ",
          "timestamp": "empty"
      },
      {
          "type": ET.ERROR,
          "timestamp": "newest-empty"
      },
  ]

  assert message_aggregator.run_cause(events) == ("usable older stderr", "usable older stderr", 0, "older")


def test_run_tool_count_matches_aggregator_rows_and_last_tool_preview() -> None:
  long_command = "x" * (message_aggregator.TOOL_PREVIEW_CHARS + 50)
  events = [
      {
          "type": ET.ASSISTANT,
          "message":
              {
                  "content":
                      [
                          {
                              "type": "tool_use",
                              "name": "Bash",
                              "input": {
                                  "command": long_command
                              }
                          },
                          {
                              "type": "tool_use",
                              "name": "ExitPlanMode",
                              "input": {
                                  "plan": "done"
                              }
                          },
                      ]
              },
          "timestamp": "assistant-time",
      },
      {
          "type": ET.TOOL_USE,
          "name": "Read",
          "input": {
              "file_path": "/tmp/file"
          },
          "timestamp": "tool-time"
      },
      {
          "type": ET.TOOL_USE,
          "name": "ExitPlanMode",
          "input": {
              "plan": "standalone event"
          },
          "timestamp": "standalone-plan-time"
      },
      {
          "type": ET.ASSISTANT,
          "message": {
              "content": [{
                  "type": "tool_use",
                  "name": "ExitPlanMode",
                  "input": {
                      "plan": "done"
                  }
              }]
          },
          "timestamp": "plan-time",
      },
  ]
  aggregator = message_aggregator.MessageAggregator(emit_stream_deltas=False)
  messages = [
      delta["message"] for delta in aggregator.feed_indexed(list(enumerate(events))) if delta["type"] == "message"
  ]
  messages.extend(delta["message"] for delta in aggregator.flush_pending())
  rendered_tool_rows = sum(len(message.get("tools", [])) for message in messages)

  assert message_aggregator.run_tool_call_count(events) == rendered_tool_rows == 3
  assert message_aggregator.run_last_tool(events) == {
      "name": "ExitPlanMode",
      "input": {
          "plan": "standalone event"
      },
      "timestamp": "standalone-plan-time",
  }
  last_bash = message_aggregator.run_last_tool(events[:1])
  assert last_bash["name"] == "Bash"
  assert len(last_bash["input"]["command"]) == message_aggregator.TOOL_PREVIEW_CHARS
