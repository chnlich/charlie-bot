from __future__ import annotations

import json

import conftest

from src.infra import event_types as ET
from src.runtime import message_aggregator
from src.runtime.api import message_utils


def test_user_event_emits_a_user_message_delta() -> None:
  agg = message_aggregator.MessageAggregator()
  deltas = list(agg.feed({
      "type": "user",
      "content": "hello",
      "timestamp": "2026-04-29T00:00:00Z",
  }))
  expected_message = {
      "role": "user",
      "content": "hello",
      "uploaded_files": [],
      "input_mode": None,
      "event_index": 0,
      "id": "legacy:0",
      "timestamp": "2026-04-29T00:00:00Z",
  }
  assert deltas == [{
      "type": "message",
      "message": expected_message,
  }]


def test_user_event_with_an_input_mode_renders_a_message_that_carries_it() -> None:
  agg = message_aggregator.MessageAggregator()

  deltas = list(
      agg.feed({
          "type": "user",
          "content": "hello",
          "input_mode": "voice",
          "timestamp": "2026-04-29T00:00:00Z",
      }))

  assert [delta["message"]["input_mode"] for delta in deltas] == ["voice"]


def test_assistant_text_then_master_done_commits_message() -> None:
  agg = message_aggregator.MessageAggregator()
  list(agg.feed({**conftest.assistant_text_event("Hi"), "timestamp": "t1"}))
  master_done_deltas = list(agg.feed({"type": "master_done", "thinking_seconds": 3, "timestamp": "t2"}))

  assert master_done_deltas == [
      {
          "type": "message",
          "message": {
              "role": "assistant",
              "content": "Hi",
              "event_index": 0,
              "id": "legacy:0",
              "timestamp": "t1",
          },
      },
      {
          "type": "message",
          "message":
              {
                  "role": "separator",
                  "thinking_seconds": 3,
                  "event_index": 1,
                  "id": "legacy:1",
                  "timestamp": "t2",
              },
      },
  ]
  assert agg.pending_draft_message() is None


def test_still_thinking_master_done_yields_separator_without_seconds() -> None:
  agg = message_aggregator.MessageAggregator()
  deltas = list(agg.feed({"type": "master_done", "still_thinking": True, "timestamp": "t1"}))

  assert deltas == [
      {
          "type": "message",
          "message":
              {
                  "role": "separator",
                  "thinking_seconds": None,
                  "event_index": 0,
                  "id": "legacy:0",
                  "timestamp": "t1",
              },
      }
  ]


def test_queued_input_renders_inside_the_turn_that_answers_it() -> None:
  # Claude-Code shape: no session_attached marker anywhere, so the stable-history
  # projection leaves the mid-round user in place. The still_thinking round end
  # renders its separator like any other, so the queued user sits in the span
  # the answering round closes instead of below the final separator.
  events = [
      {
          "type": ET.USER,
          "content": "first question",
          "timestamp": "t1"
      },
      {
          **conftest.assistant_text_event("first answer"), "timestamp": "t2"
      },
      {
          "type": ET.USER,
          "content": "second question",
          "timestamp": "t3"
      },
      {
          **conftest.assistant_text_event("mid-round answer"), "timestamp": "t4"
      },
      {
          "type": ET.MASTER_DONE,
          "still_thinking": True,
          "timestamp": "t5"
      },
      {
          **conftest.assistant_text_event("second answer"), "timestamp": "t6"
      },
      {
          "type": ET.MASTER_DONE,
          "thinking_seconds": 7,
          "timestamp": "t7"
      },
  ]

  messages = message_utils.events_to_messages(events)
  view_messages, pending = message_utils.events_to_view(events)

  assert [message["role"] for message in messages] == [
      "user",
      "assistant",
      "user",
      "assistant",
      "separator",
      "assistant",
      "separator",
  ]
  assert [message["id"] for message in messages] == [f"legacy:{index}" for index in range(7)]
  assert messages[4]["thinking_seconds"] is None
  assert messages[6]["thinking_seconds"] == 7
  assert view_messages == messages
  assert pending is None


def test_task_delegated_message_exposes_metadata_without_full_description_body() -> None:
  agg = message_aggregator.MessageAggregator()
  long_description = "## Goal\nDo a long task spec that belongs in Workers."
  invocation = conftest.delegate_invocation(task_spec_file="/tmp/task.md", reviewer_context_file="/tmp/reviewer.md")

  deltas = list(
      agg.feed(
          {
              "type": ET.TASK_DELEGATED,
              "thread_id": "thread-id",
              "description": long_description,
              "timestamp": "2026-07-01T12:00:00Z",
              "backend": "codex-o3",
              "model": "o3",
              "delegate_invocation": invocation,
          }))

  assert deltas == [
      {
          "type": "message",
          "message":
              {
                  "role": "task_delegated",
                  "content": "Task delegated",
                  "thread_id": "thread-id",
                  "child_session_id": "",
                  "delegate_invocation": invocation,
                  "backend": "codex-o3",
                  "model": "o3",
                  "event_index": 0,
                  "id": "legacy:0",
                  "timestamp": "2026-07-01T12:00:00Z",
              },
      }
  ]
  # The description body stays on the persisted event; no bubble text carries it.
  assert long_description not in deltas[0]["message"]["content"]
  assert "description" not in deltas[0]["message"]


def test_tool_use_attaches_to_buffer_then_tool_result_updates_output() -> None:
  agg = message_aggregator.MessageAggregator()
  list(agg.feed(conftest.assistant_text_tool_use_event("Running", "Bash", {"command": "ls"}, "t1")))
  # Internal CC tool_result event arrives as a user event with `message` only.
  list(
      agg.feed(
          {
              "type": "user",
              "message": {
                  "content": [{
                      "type": "tool_result",
                      "content": "file1\nfile2",
                      "is_error": False
                  }]
              },
          }))
  draft = agg.pending_draft_message()
  assert draft is not None
  assert draft["content"] == "Running"
  assert draft["tools"] == [{
      "name": "Bash",
      "input": {
          "command": "ls"
      },
      "output": "file1\nfile2",
      "is_error": False,
  }]


def test_thinking_is_flushed_with_assistant_draft() -> None:
  agg = message_aggregator.MessageAggregator()
  list(agg.feed({**conftest.assistant_text_event("Hi"), "timestamp": "t1"}))
  list(agg.feed({"type": "thinking", "content": "planning", "timestamp": "t2"}))
  deltas = list(agg.feed({"type": "master_done", "thinking_seconds": 1, "timestamp": "t3"}))

  assert deltas[0]["message"]["role"] == "assistant"
  assert deltas[0]["message"]["content"] == "Hi"
  assert deltas[0]["message"]["thinking"] == "planning"
  assert deltas[1]["message"]["role"] == "separator"


def test_stable_history_orders_queued_user_between_completed_runs() -> None:
  events = conftest.queued_user_reorder_events()

  messages = message_utils.events_to_messages(events)
  view_messages, pending = message_utils.events_to_view(events)

  assert view_messages == messages
  assert pending is None
  assert [message["role"] for message in messages] == ["assistant", "separator", "user", "assistant", "separator"]
  assert [message["id"] for message in messages] == ["assistant-1", "done-1", "queued-user", "assistant-2", "done-2"]
  assert messages[0]["thinking"] == "final thought"
  assert messages[0]["tools"][0]["output"] == "report contents"


def test_stream_deltas_stay_bounded_after_a_giant_tool_result() -> None:
  agg = message_aggregator.MessageAggregator()
  list(agg.feed({"type": ET.TOOL_USE, "name": "Bash", "input": {"cmd": "cat big.log"}}))
  list(agg.feed({"type": ET.TOOL_RESULT, "tool_name": "Bash", "content": "z" * 10_000_000}))

  serialized = []
  for i in range(20):
    list(agg.feed(conftest.assistant_text_event(f"delta {i}")))
    draft = agg.pending_draft_message()
    serialized.append(len(json.dumps(draft)))

  # Every live delta re-serializes the whole buffered draft, so one uncapped
  # output would ride all of them; the cap bounds each snapshot instead.
  assert max(serialized) < 100_000
