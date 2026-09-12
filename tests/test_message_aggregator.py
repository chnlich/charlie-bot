from __future__ import annotations

import json
from copy import deepcopy

import pytest
from conftest import assistant_event as _assistant_event
from conftest import assistant_text_tool_use_event as _assistant_text_tool_use_event
from conftest import queued_user_reorder_events as _reorder_events

from src.api.message_utils import events_to_messages, events_to_view
from src.core import event_types as ET
from src.core.message_aggregator import TOOL_OUTPUT_RENDER_CAP, TOOL_PREVIEW_CHARS, MessageAggregator

VOICE_KEY = "is_" + "voice"


def test_user_event_emits_a_user_message_delta() -> None:
  agg = MessageAggregator()
  deltas = list(agg.feed({
      "type": "user",
      "content": "hello",
      "timestamp": "2026-04-29T00:00:00Z",
  }))
  expected_message = {
      "role": "user",
      "content": "hello",
      "uploaded_files": [],
      "event_index": 0,
      "id": "legacy:0",
      "timestamp": "2026-04-29T00:00:00Z",
  }
  expected_message[VOICE_KEY] = False
  assert deltas == [{
      "type": "message",
      "message": expected_message,
  }]


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
  # The description body stays on the persisted event; no bubble text carries it.
  assert long_description not in deltas[0]["message"]["content"]
  assert "description" not in deltas[0]["message"]


def test_tool_use_attaches_to_buffer_then_tool_result_updates_output() -> None:
  agg = MessageAggregator()
  list(agg.feed(_assistant_text_tool_use_event("Running", "Bash", {"command": "ls"}, "t1")))
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


def test_flat_tool_use_aggregates_into_tools() -> None:
  agg = MessageAggregator()
  list(agg.feed({"type": ET.TOOL_USE, "name": "Bash", "input": {"cmd": "ls"}}))

  draft = agg.pending_draft_message()
  assert draft is not None
  assert draft["tools"] == [{
      "name": "Bash",
      "input": {
          "cmd": "ls"
      },
      "output": "",
      "is_error": False,
  }]


def test_flat_tool_result_attaches_to_last_tool() -> None:
  agg = MessageAggregator()
  list(agg.feed({"type": ET.TOOL_USE, "name": "Bash", "input": {"cmd": "pwd"}}))
  list(agg.feed({"type": ET.TOOL_RESULT, "tool_name": "Bash", "content": "/home", "is_error": True}))

  draft = agg.pending_draft_message()
  assert draft is not None
  assert draft["tools"][0]["output"] == "/home"
  assert draft["tools"][0]["is_error"] is True


def test_text_and_flat_tools_interleave() -> None:
  agg = MessageAggregator()
  list(agg.feed({"type": ET.ASSISTANT, "message": {"content": [{"type": "text", "text": "hi "}]}}))
  list(agg.feed({"type": ET.TOOL_USE, "name": "Bash", "input": {"cmd": "ls"}}))
  list(agg.feed({"type": ET.TOOL_RESULT, "tool_name": "Bash", "content": "/home"}))
  deltas = list(agg.feed({"type": ET.ASSISTANT, "message": {"content": [{"type": "text", "text": "done"}]}}))

  finalized = [delta for delta in deltas if delta["type"] == "message"]
  assert len(finalized) == 1
  message = finalized[0]["message"]
  assert message["content"] == "hi "
  assert len(message["tools"]) == 1
  assert message["tools"][0]["output"] == "/home"

  draft = agg.pending_draft_message()
  assert draft is not None
  assert draft["content"] == "done"
  assert "tools" not in draft or draft["tools"] == []


def test_flat_tool_result_with_empty_buf_is_silent() -> None:
  agg = MessageAggregator()
  deltas = list(agg.feed({"type": ET.TOOL_RESULT, "tool_name": "Bash", "content": "orphan"}))

  assert not deltas
  assert agg.pending_draft_message() is None


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
                      }],
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


@pytest.mark.parametrize(
    ("event", "expected_content", "expected_kind"),
    [
        pytest.param(
            {
                "type": "system",
                "subtype": "tui_menu_dismissed",
                "content": "Warning: dismissed a Claude TUI startup menu with Escape before sending the prompt.",
                "timestamp": "t",
            },
            "Warning: dismissed a Claude TUI startup menu with Escape before sending the prompt.",
            None,
            id="tui_menu_dismissed",
        ),
        pytest.param(
            {
                "type": "context_compacted",
                "trigger": "manual",
                "pre_tokens": 21988,
                "timestamp": "t",
            },
            "Context compacted (manual) — was 22k tokens",
            "context_compacted",
            id="context_compacted",
        ),
        pytest.param(
            {
                "type": "context_compact_failed",
                "error": "context too large",
                "timestamp": "t",
            },
            "Compaction failed — context too large",
            "context_compact_failed",
            id="compact_failed_with_error",
        ),
        pytest.param(
            {
                "type": "context_compact_failed",
                "error": None,
                "timestamp": "t",
            },
            "Compaction failed",
            "context_compact_failed",
            id="compact_failed_without_error",
        ),
        pytest.param(
            {
                "type": ET.BACKEND_SWITCHED,
                "from": "claude-opus-5",
                "to": "claude-fable-5",
                "timestamp": "t",
            },
            "Backend switched: claude-opus-5 → claude-fable-5",
            None,
            id="backend_switched",
        ),
    ],
)
def test_system_event_projects_to_one_system_message(
    event: dict, expected_content: str, expected_kind: str | None) -> None:
  agg = MessageAggregator()
  deltas = list(agg.feed(event))

  expected_message = {
      "role": "system",
      "content": expected_content,
      "event_index": 0,
      "id": "legacy:0",
      "timestamp": "t",
  }
  if expected_kind is not None:
    expected_message["kind"] = expected_kind
  assert deltas == [{
      "type": "message",
      "message": expected_message,
  }]


def test_init_system_event_is_ignored() -> None:
  agg = MessageAggregator()
  list(agg.feed({"type": "assistant", "message": {"content": [{"type": "text", "text": "draft"}]}, "timestamp": "t1"}))

  assert not list(agg.feed({"type": "system", "subtype": "init", "timestamp": "t2"}))
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
  assert not list(agg.flush_pending())


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
      agg.feed(
          {
              "type": "assistant",
              "message":
                  {
                      "content":
                          [
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
          "message":
              {
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
  list(
      agg.feed(
          {
              "type": "assistant",
              "message": {
                  "content": [{
                      "type": "thinking",
                      "thinking": "Step"
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
                      "type": "thinking",
                      "thinking": "Step two"
                  }]
              },
              "timestamp": "t2"
          }))

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


def test_stable_history_orders_queued_user_between_completed_runs() -> None:
  events = _reorder_events()

  messages = events_to_messages(events)
  view_messages, pending = events_to_view(events)

  assert view_messages == messages
  assert pending is None
  assert [message["role"] for message in messages] == ["assistant", "separator", "user", "assistant", "separator"]
  assert [message["id"] for message in messages] == ["assistant-1", "done-1", "queued-user", "assistant-2", "done-2"]
  assert messages[0]["thinking"] == "final thought"
  assert messages[0]["tools"][0]["output"] == "report contents"


def test_stable_history_preserves_still_thinking_separator_semantics() -> None:
  for still_thinking, expected_roles in [
      (False, ["assistant", "separator", "user"]),
      (True, ["assistant", "user"]),
  ]:
    events = [
        {
            "session_id": "opencode-session"
        },
        _assistant_event("conclusion"),
        {
            "type": ET.USER,
            "content": "queued"
        },
        {
            "type": ET.MASTER_DONE,
            "still_thinking": still_thinking
        },
    ]

    assert [message["role"] for message in events_to_messages(events)] == expected_roles


def test_stable_history_preserves_multiple_deferred_user_source_order() -> None:
  events = [
      {
          "session_id": "opencode-session"
      },
      _assistant_event("conclusion"),
      {
          "id": "user-1",
          "type": ET.USER,
          "content": "first queued user"
      },
      {
          "id": "user-2",
          "type": ET.USER,
          "content": "second queued user"
      },
      {
          "type": ET.MASTER_DONE
      },
  ]

  messages = events_to_messages(events)

  assert [message["role"] for message in messages] == ["assistant", "separator", "user", "user"]
  assert [message["id"] for message in messages if message["role"] == "user"] == ["user-1", "user-2"]


def test_stable_history_leaves_tool_result_and_slash_users_on_immediate_path() -> None:
  events = [
      {
          "session_id": "opencode-session"
      },
      _assistant_event("before slash", "assistant-before-slash"),
      {
          "id": "tool",
          "type": ET.TOOL_USE,
          "name": "Read",
          "input": {
              "file_path": "a.txt"
          }
      },
      {
          "type": ET.USER,
          "message": {
              "content": [{
                  "type": "tool_result",
                  "content": "tool output"
              }]
          }
      },
      {
          "id": "slash-user",
          "type": ET.USER,
          "content": "/status"
      },
      _assistant_event("after slash", "assistant-after-slash"),
      {
          "id": "done",
          "type": ET.MASTER_DONE
      },
  ]
  live = MessageAggregator()
  live_messages = [delta["message"] for delta in live.feed_all(events) if delta["type"] == "message"]

  messages = events_to_messages(events)

  assert messages == live_messages
  assert [message["role"] for message in messages] == ["assistant", "user", "assistant", "separator"]
  assert messages[0]["tools"][0]["output"] == "tool output"
  assert messages[1]["content"] == "/status"


def test_stable_history_does_not_repair_incomplete_windows() -> None:
  cases = [
      (
          [
              _assistant_event("answer"),
              {
                  "id": "user",
                  "type": ET.USER,
                  "content": "next"
              },
              {
                  "id": "done",
                  "type": ET.MASTER_DONE
              },
          ],
          ["assistant", "user", "separator"],
      ),
      (
          [
              {
                  "session_id": "opencode-session"
              },
              _assistant_event("answer"),
              {
                  "id": "user",
                  "type": ET.USER,
                  "content": "next"
              },
          ],
          ["assistant", "user"],
      ),
  ]

  for events, expected_roles in cases:
    messages = events_to_messages(events)
    view_messages, pending = events_to_view(events)
    assert [message["role"] for message in messages] == expected_roles
    assert [message["role"] for message in view_messages] == expected_roles
    assert [message["id"] for message in messages].count("user") == 1
    assert pending is None


def test_stable_history_preserves_deferred_user_metadata_without_mutating_events() -> None:
  events = [
      {
          "session_id": "opencode-session",
          "timestamp": "start"
      },
      _assistant_event("conclusion") | {
          "timestamp": "assistant-ts"
      },
      {
          "id": "queued-user",
          "type": ET.USER,
          "content": "voice question",
          "timestamp": "user-ts",
          "is_voice": True,
          "uploaded_files": [{
              "filename": "trace.json",
              "path": "/tmp/trace.json",
              "size": 42,
          }],
      },
      {
          "id": "done",
          "type": ET.MASTER_DONE,
          "timestamp": "done-ts"
      },
  ]
  original = deepcopy(events)

  messages = events_to_messages(events, event_index_offset=40)
  events_to_view(events, event_index_offset=40)
  user = next(message for message in messages if message["role"] == "user")

  expected = {
      "role": "user",
      "content": "voice question",
      "uploaded_files": [{
          "filename": "trace.json",
          "path": "/tmp/trace.json",
          "size": 42,
      }],
      "event_index": 42,
      "id": "queued-user",
      "timestamp": "user-ts",
  }
  expected[VOICE_KEY] = True
  assert user == expected
  assert events == original


def _mixed_event_sequence() -> list[dict]:
  return [
      {
          "type": ET.USER,
          "message": {
              "content": [{
                  "type": "text",
                  "text": "run it"
              }]
          },
          "timestamp": "t0"
      },
      {
          "type": ET.ASSISTANT,
          "message": {
              "content": [{
                  "type": "text",
                  "text": "working "
              }]
          },
          "timestamp": "t1"
      },
      {
          "type": ET.THINKING,
          "content": "considering",
          "timestamp": "t2"
      },
      {
          "type": ET.TOOL_USE,
          "name": "Bash",
          "input": {
              "cmd": "ls"
          },
          "timestamp": "t3"
      },
      {
          "type": ET.TOOL_RESULT,
          "tool_name": "Bash",
          "content": "file.txt",
          "timestamp": "t4"
      },
      {
          "type": ET.ASSISTANT,
          "message": {
              "content": [{
                  "type": "text",
                  "text": "done"
              }]
          },
          "timestamp": "t5"
      },
      {
          "type": ET.MASTER_DONE,
          "thinking_seconds": 1,
          "timestamp": "t6"
      },
      {
          "type": ET.ASSISTANT,
          "message": {
              "content": [{
                  "type": "text",
                  "text": "tail draft"
              }]
          },
          "timestamp": "t7"
      },
  ]


def test_emit_stream_deltas_false_keeps_message_deltas_and_pending_identical() -> None:
  events = _mixed_event_sequence()

  live = MessageAggregator()
  live_deltas = [delta for ev in events for delta in live.feed(ev)]
  quiet = MessageAggregator(emit_stream_deltas=False)
  quiet_deltas = [delta for ev in events for delta in quiet.feed(ev)]

  assert [d for d in quiet_deltas if d["type"] == "stream"] == []
  assert [d for d in live_deltas if d["type"] == "message"] == [d for d in quiet_deltas if d["type"] == "message"]
  assert live.pending_draft_message() == quiet.pending_draft_message()


def test_clone_preserves_emit_stream_deltas() -> None:
  assert MessageAggregator().clone().emit_stream_deltas is True
  assert MessageAggregator(emit_stream_deltas=False).clone().emit_stream_deltas is False


def test_flat_tool_result_output_is_capped_with_marker() -> None:
  agg = MessageAggregator()
  list(agg.feed({"type": ET.TOOL_USE, "name": "Bash", "input": {"cmd": "dump"}}))
  giant = "x" * (TOOL_OUTPUT_RENDER_CAP + 5000)
  list(agg.feed({"type": ET.TOOL_RESULT, "tool_name": "Bash", "content": giant}))

  draft = agg.pending_draft_message()
  assert draft["tools"][0]["output"] == "x" * TOOL_OUTPUT_RENDER_CAP
  assert draft["tools"][0]["output_truncated"] is True


def test_cc_tool_result_output_is_capped_with_marker() -> None:
  agg = MessageAggregator()
  list(agg.feed(_assistant_text_tool_use_event("Running", "Bash", {"command": "ls"}, "t1")))
  giant = "y" * (TOOL_OUTPUT_RENDER_CAP + 1)
  list(
      agg.feed(
          {
              "type": "user",
              "message": {
                  "content": [{
                      "type": "tool_result",
                      "content": giant,
                      "is_error": False
                  }]
              },
          }))

  draft = agg.pending_draft_message()
  assert draft["tools"][0]["output"] == "y" * TOOL_OUTPUT_RENDER_CAP
  assert draft["tools"][0]["output_truncated"] is True


def test_tool_output_under_cap_carries_no_marker() -> None:
  agg = MessageAggregator()
  list(agg.feed({"type": ET.TOOL_USE, "name": "Bash", "input": {"cmd": "ls"}}))
  list(agg.feed({"type": ET.TOOL_RESULT, "tool_name": "Bash", "content": "file1\nfile2"}))

  draft = agg.pending_draft_message()
  assert draft["tools"][0] == {
      "name": "Bash",
      "input": {
          "cmd": "ls"
      },
      "output": "file1\nfile2",
      "is_error": False,
  }


def test_stream_deltas_stay_bounded_after_a_giant_tool_result() -> None:
  agg = MessageAggregator()
  list(agg.feed({"type": ET.TOOL_USE, "name": "Bash", "input": {"cmd": "cat big.log"}}))
  list(agg.feed({"type": ET.TOOL_RESULT, "tool_name": "Bash", "content": "z" * (TOOL_OUTPUT_RENDER_CAP * 500)}))

  serialized = []
  for i in range(20):
    list(agg.feed({"type": ET.ASSISTANT, "message": {"content": [{"type": "text", "text": f"delta {i}"}]}}))
    draft = agg.pending_draft_message()
    serialized.append(len(json.dumps(draft)))

  # Every live delta re-serializes the whole buffered draft, so one uncapped
  # output would ride all of them; the cap bounds each snapshot instead.
  assert max(serialized) < TOOL_OUTPUT_RENDER_CAP + 100_000


def test_stream_delta_tool_rows_carry_the_preview_shape() -> None:
  agg = MessageAggregator()
  list(agg.feed({"type": ET.TOOL_USE, "name": "Write", "input": {"file_path": "/tmp/a", "content": "c" * 5000}}))
  list(agg.feed({"type": ET.TOOL_RESULT, "tool_name": "Write", "content": "o" * (TOOL_OUTPUT_RENDER_CAP + 1)}))
  stream = next(d for d in agg.feed({"type": ET.THINKING, "content": "t"}) if d["type"] == "stream")

  tool = stream["message"]["tools"][0]
  assert tool["output"] == "o" * TOOL_PREVIEW_CHARS
  assert tool["output_truncated"] is True
  assert tool["input"]["content"] == "c" * TOOL_PREVIEW_CHARS
  assert tool["input_truncated"] is True
  assert tool["input"]["file_path"] == "/tmp/a"


def test_stream_delta_leaves_the_committed_shape_at_the_render_cap() -> None:
  agg = MessageAggregator()
  list(agg.feed({"type": ET.TOOL_USE, "name": "Bash", "input": {"command": "x" * 5000}}))
  list(agg.feed({"type": ET.TOOL_RESULT, "tool_name": "Bash", "content": "y" * (TOOL_OUTPUT_RENDER_CAP + 1)}))

  commit = next(d for d in agg.flush_pending() if d["type"] == "message")
  tool = commit["message"]["tools"][0]
  assert tool["output"] == "y" * TOOL_OUTPUT_RENDER_CAP
  assert tool["output_truncated"] is True
  assert "input_truncated" not in tool
  assert tool["input"] == {"command": "x" * 5000}


def test_stream_delta_shares_no_trimmed_state_with_the_buffer() -> None:
  agg = MessageAggregator()
  list(agg.feed({"type": ET.TOOL_USE, "name": "Bash", "input": {"command": "z" * 5000}}))
  list(agg.feed({"type": ET.TOOL_RESULT, "tool_name": "Bash", "content": "w" * 5000}))
  first = next(d for d in agg.feed({"type": ET.THINKING, "content": "t"}) if d["type"] == "stream")
  second = next(d for d in agg.feed({"type": ET.THINKING, "content": "t2"}) if d["type"] == "stream")

  # Each stream snapshot trims its own copies: the buffer the commit reads and
  # the earlier snapshots' bytes stay untouched.
  assert first["message"]["tools"][0]["output"] == "w" * TOOL_PREVIEW_CHARS
  assert second["message"]["tools"][0]["output"] == "w" * TOOL_PREVIEW_CHARS
  commit = next(d for d in agg.flush_pending() if d["type"] == "message")
  assert commit["message"]["tools"][0]["output"] == "w" * 5000
