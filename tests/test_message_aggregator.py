from __future__ import annotations

from src.core import event_types as ET
from src.core.message_aggregator import MessageAggregator


def test_user_event_emits_a_user_message_delta() -> None:
  agg = MessageAggregator()
  deltas = list(
      agg.feed({
          "type": "user",
          "content": "hello",
          "timestamp": "2026-04-29T00:00:00Z",
          "is_voice": False,
      }))
  assert deltas == [
      {
          "type": "message",
          "message":
              {
                  "role": "user",
                  "content": "hello",
                  "uploaded_files": [],
                  "is_voice": False,
                  "event_index": 0,
                  "id": "legacy:0",
                  "timestamp": "2026-04-29T00:00:00Z",
              },
      }
  ]


def test_assistant_text_event_emits_a_stream_delta() -> None:
  agg = MessageAggregator()
  deltas = list(
      agg.feed(
          {
              "type": "assistant",
              "message": {
                  "content": [{
                      "type": "text",
                      "text": "Hello "
                  }]
              },
              "timestamp": "2026-04-29T00:00:00Z",
          }))
  assert deltas == [
      {
          "type": "stream",
          "message":
              {
                  "role": "assistant",
                  "content": "Hello ",
                  "event_index": 0,
                  "id": "legacy:0",
                  "timestamp": "2026-04-29T00:00:00Z",
              },
      }
  ]
  assert agg.pending_draft_message() == deltas[0]["message"]


def test_assistant_text_then_master_done_commits_message() -> None:
  agg = MessageAggregator()
  list(agg.feed({"type": "assistant", "message": {"content": [{"type": "text", "text": "Hi"}]}, "timestamp": "t1"}))
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


def test_master_done_with_still_thinking_skips_separator() -> None:
  agg = MessageAggregator()
  list(agg.feed({"type": "assistant", "message": {"content": [{"type": "text", "text": "Hi"}]}, "timestamp": "t1"}))
  deltas = list(agg.feed({"type": "master_done", "still_thinking": True, "timestamp": "t2"}))
  # Pending assistant draft commits, but no separator is emitted.
  assert deltas == [
      {
          "type": "message",
          "message": {
              "role": "assistant",
              "content": "Hi",
              "event_index": 0,
              "id": "legacy:0",
              "timestamp": "t1",
          },
      }
  ]


def test_task_delegated_message_exposes_metadata_without_full_description_body() -> None:
  agg = MessageAggregator()
  long_description = "## Goal\nDo a long task spec that belongs in Workers."

  deltas = list(
      agg.feed(
          {
              "type": ET.TASK_DELEGATED,
              "thread_id": "thread-id",
              "description": long_description,
              "timestamp": "2026-07-01T12:00:00Z",
              "backend": "codex-o3",
              "model": "o3",
              "delegate_invocation":
                  {
                      "task_type": "implement",
                      "repo_path": "/tmp/repo",
                      "base_branch": "main",
                      "task_spec_file": "/tmp/task.md",
                      "reviewer_context_file": "/tmp/reviewer.md",
                      "keep_worktree": False,
                      "backend": "codex-o3",
                  },
          }))

  assert deltas == [
      {
          "type": "message",
          "message":
              {
                  "role": "task_delegated",
                  "content": "Task delegated",
                  "thread_id": "thread-id",
                  "description": long_description,
                  "delegate_invocation":
                      {
                          "task_type": "implement",
                          "repo_path": "/tmp/repo",
                          "base_branch": "main",
                          "task_spec_file": "/tmp/task.md",
                          "reviewer_context_file": "/tmp/reviewer.md",
                          "keep_worktree": False,
                          "backend": "codex-o3",
                      },
                  "backend": "codex-o3",
                  "model": "o3",
                  "event_index": 0,
                  "id": "legacy:0",
                  "timestamp": "2026-07-01T12:00:00Z",
              },
      }
  ]
  assert long_description not in deltas[0]["message"]["content"]


def test_tool_use_attaches_to_buffer_then_tool_result_updates_output() -> None:
  agg = MessageAggregator()
  list(
      agg.feed(
          {
              "type": "assistant",
              "message":
                  {
                      "content":
                          [
                              {
                                  "type": "text",
                                  "text": "Running"
                              },
                              {
                                  "type": "tool_use",
                                  "name": "Bash",
                                  "input": {
                                      "command": "ls"
                                  }
                              },
                          ],
                  },
              "timestamp": "t1",
          }))
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


def test_exit_plan_mode_emits_plan_message_with_explicit_text() -> None:
  agg = MessageAggregator()
  deltas = list(
      agg.feed(
          {
              "type": "assistant",
              "message":
                  {
                      "content": [{
                          "type": "tool_use",
                          "name": "ExitPlanMode",
                          "input": {
                              "plan": "Step 1\nStep 2"
                          }
                      },],
                  },
              "timestamp": "t1",
          }))
  assert deltas == [
      {
          "type": "message",
          "message":
              {
                  "role": "plan",
                  "content": "Step 1\nStep 2",
                  "event_index": 0,
                  "id": "legacy:0",
                  "timestamp": "t1",
              },
      }
  ]


def test_exit_plan_mode_without_explicit_plan_promotes_buffer() -> None:
  agg = MessageAggregator()
  list(
      agg.feed(
          {
              "type": "assistant",
              "message": {
                  "content": [{
                      "type": "text",
                      "text": "Plan body"
                  }]
              },
              "timestamp": "t1"
          }))
  deltas = list(
      agg.feed(
          {
              "type": "assistant",
              "message": {
                  "content": [{
                      "type": "tool_use",
                      "name": "ExitPlanMode",
                      "input": {}
                  }]
              },
              "timestamp": "t2",
          }))
  # Buffered assistant text is repromoted as a plan message; no second
  # `message` for the assistant draft is emitted.
  assert deltas[0] == {
      "type": "message",
      "message": {
          "role": "plan",
          "content": "Plan body",
          "event_index": 1,
          "id": "legacy:1",
          "timestamp": "t1",
      },
  }


def test_consecutive_assistant_text_events_split_into_separate_bubbles() -> None:
  agg = MessageAggregator()
  list(agg.feed({"type": "assistant", "message": {"content": [{"type": "text", "text": "First"}]}, "timestamp": "t1"}))
  deltas = list(
      agg.feed({
          "type": "assistant",
          "message": {
              "content": [{
                  "type": "text",
                  "text": "Second"
              }]
          },
          "timestamp": "t2"
      }))
  # First bubble is committed before the second buffer starts; matches the
  # legacy events_to_messages flushing rule.
  commit = deltas[0]
  stream = deltas[1]
  # The flushed message carries the current (flushing) event's index, matching
  # the long-standing events_to_messages behavior where last_event_idx is
  # updated before the flush check fires.
  assert commit == {
      "type": "message",
      "message": {
          "role": "assistant",
          "content": "First",
          "event_index": 1,
          "id": "legacy:0",
          "timestamp": "t1",
      },
  }
  assert stream == {
      "type": "stream",
      "message": {
          "role": "assistant",
          "content": "Second",
          "event_index": 1,
          "id": "legacy:1",
          "timestamp": "t2",
      },
  }


def test_handler_result_flushes_draft_and_emits_system_message() -> None:
  agg = MessageAggregator()
  list(
      agg.feed({
          "type": "assistant",
          "message": {
              "content": [{
                  "type": "text",
                  "text": "Working"
              }]
          },
          "timestamp": "t1"
      }))
  deltas = list(
      agg.feed({
          "type": "handler_result",
          "task": "Lint",
          "message": "All clean",
          "status": "ok",
          "timestamp": "t2"
      }))
  assert [d["message"]["role"] for d in deltas] == ["assistant", "system"]
  assert deltas[1]["message"]["content"] == "✓ Lint: All clean"


def test_tui_menu_dismissed_system_event_emits_system_message() -> None:
  agg = MessageAggregator()
  deltas = list(
      agg.feed({
          "type": "system",
          "subtype": "tui_menu_dismissed",
          "content": "Warning: dismissed a Claude TUI startup menu with Escape before sending the prompt.",
          "timestamp": "t",
      }))

  assert deltas == [
      {
          "type": "message",
          "message":
              {
                  "role": "system",
                  "content": "Warning: dismissed a Claude TUI startup menu with Escape before sending the prompt.",
                  "event_index": 0,
                  "id": "legacy:0",
                  "timestamp": "t",
              },
      }
  ]


def test_init_system_event_is_ignored() -> None:
  agg = MessageAggregator()
  list(agg.feed({"type": "assistant", "message": {"content": [{"type": "text", "text": "draft"}]}, "timestamp": "t1"}))

  assert list(agg.feed({"type": "system", "subtype": "init", "timestamp": "t2"})) == []
  assert agg.pending_draft_message()["content"] == "draft"


def test_flush_pending_emits_dangling_draft() -> None:
  agg = MessageAggregator()
  list(agg.feed({"type": "assistant", "message": {"content": [{"type": "text", "text": "tail"}]}, "timestamp": "t"}))
  flushed = list(agg.flush_pending())
  assert flushed == [
      {
          "type": "message",
          "message": {
              "role": "assistant",
              "content": "tail",
              "event_index": 0,
              "id": "legacy:0",
              "timestamp": "t",
          },
      }
  ]
  assert agg.pending_draft_message() is None
  assert list(agg.flush_pending()) == []


def test_pending_draft_message_is_a_pure_snapshot() -> None:
  agg = MessageAggregator()
  list(agg.feed({"type": "assistant", "message": {"content": [{"type": "text", "text": "snap"}]}, "timestamp": "t"}))
  first = agg.pending_draft_message()
  second = agg.pending_draft_message()
  assert first == second
  # Mutating the snapshot must not corrupt internal state.
  first["content"] = "tampered"
  assert agg.pending_draft_message()["content"] == "snap"


def test_event_index_offset_is_applied() -> None:
  agg = MessageAggregator(event_index_offset=10)
  deltas = list(agg.feed({"type": "user", "content": "hi", "timestamp": "t"}))
  assert deltas[0]["message"]["event_index"] == 10
  assert deltas[0]["message"]["id"] == "legacy:10"


def test_event_id_is_propagated_when_present() -> None:
  agg = MessageAggregator()
  deltas = list(agg.feed({"id": "event-uuid", "type": "user", "content": "hi", "timestamp": "t"}))
  assert deltas[0]["message"]["id"] == "event-uuid"


def test_thinking_delta_appends_to_assistant_draft() -> None:
  agg = MessageAggregator()
  list(agg.feed({"type": "assistant", "message": {"content": [{"type": "text", "text": "Hi"}]}, "timestamp": "t1"}))
  deltas = list(agg.feed({"type": "thinking", "content": "planning", "timestamp": "t2"}))

  assert [d["type"] for d in deltas] == ["stream"]
  assert deltas[0]["message"]["role"] == "assistant"
  assert deltas[0]["message"]["content"] == "Hi"
  assert deltas[0]["message"]["thinking"] == "planning"
  assert agg.pending_draft_message() == deltas[0]["message"]


def test_cc_assistant_thinking_block_attaches_to_draft() -> None:
  agg = MessageAggregator()
  deltas = list(
      agg.feed({
          "type": "assistant",
          "message": {
              "content": [
                  {
                      "type": "thinking",
                      "thinking": "Step one",
                      "signature": "sig1"
                  },
                  {
                      "type": "text",
                      "text": "Hello"
                  },
              ]
          },
          "timestamp": "t1",
      }))

  assert deltas == [
      {
          "type": "stream",
          "message": {
              "role": "assistant",
              "content": "Hello",
              "thinking": "Step one",
              "event_index": 0,
              "id": "legacy:0",
              "timestamp": "t1",
          },
      }
  ]
  assert "signature" not in deltas[0]["message"]


def test_cc_thinking_snapshot_is_cumulative() -> None:
  agg = MessageAggregator()
  list(agg.feed({"type": "assistant", "message": {"content": [{"type": "thinking", "thinking": "Step"}]}, "timestamp": "t1"}))
  deltas = list(
      agg.feed({"type": "assistant", "message": {"content": [{"type": "thinking", "thinking": "Step two"}]}, "timestamp": "t2"}))

  assert deltas[0]["type"] == "stream"
  assert deltas[0]["message"]["thinking"] == "Step two"
  assert deltas[0]["message"]["content"] == ""


def test_thinking_is_flushed_with_assistant_draft() -> None:
  agg = MessageAggregator()
  list(agg.feed({"type": "assistant", "message": {"content": [{"type": "text", "text": "Hi"}]}, "timestamp": "t1"}))
  list(agg.feed({"type": "thinking", "content": "planning", "timestamp": "t2"}))
  deltas = list(agg.feed({"type": "master_done", "thinking_seconds": 1, "timestamp": "t3"}))

  assert deltas[0]["message"]["role"] == "assistant"
  assert deltas[0]["message"]["content"] == "Hi"
  assert deltas[0]["message"]["thinking"] == "planning"
  assert deltas[1]["message"]["role"] == "separator"


def test_thinking_event_alone_creates_assistant_draft() -> None:
  agg = MessageAggregator()
  deltas = list(agg.feed({"type": "thinking", "content": "reasoning", "timestamp": "t"}))

  assert deltas[0]["message"]["role"] == "assistant"
  assert deltas[0]["message"]["content"] == ""
  assert deltas[0]["message"]["thinking"] == "reasoning"
