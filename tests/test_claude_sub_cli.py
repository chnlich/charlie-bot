import pytest

from src.cli import claude_sub


def test_parse_argv_accepts_cc_backend_shape_with_multiline_prompt() -> None:
  args = claude_sub.parse_argv(
      [
          "-p",
          "--output-format",
          "stream-json",
          "--verbose",
          "--dangerously-skip-permissions",
          "--disallowed-tools",
          "Monitor,CronCreate",
          "--model",
          "claude-opus-4-8",
          "--effort",
          "max",
          "--resume",
          "session-id",
          "--",
          "hello\nworld",
      ])

  assert args.output_format == "stream-json"
  assert args.prompt == "hello\nworld"
  assert args.model == "claude-opus-4-8"
  assert args.effort == "max"
  assert args.resume == "session-id"
  assert args.disallowed_tools == ["Monitor,CronCreate"]


def test_parse_argv_rejects_non_stream_json_output() -> None:
  with pytest.raises(ValueError, match="stream-json"):
    claude_sub.parse_argv(["-p", "--output-format", "json", "--", "hello"])


def test_convert_record_suppresses_typed_user_prompt() -> None:
  event = claude_sub._convert_record(
      {
          "type": "user",
          "message": {
              "role": "user",
              "content": "hello",
          },
          "sessionId": "session-id",
      },
      "session-id",
  )

  assert event is None


def test_convert_record_preserves_tool_result_user_event() -> None:
  event = claude_sub._convert_record(
      {
          "type": "user",
          "message":
              {
                  "role": "user",
                  "content": [{
                      "type": "tool_result",
                      "tool_use_id": "toolu_123",
                      "content": "ok",
                  }],
              },
          "toolUseResult": {
              "stdout": "ok"
          },
          "sessionId": "session-id",
          "uuid": "event-id",
      },
      "session-id",
  )

  assert event == {
      "type": "user",
      "message": {
          "role": "user",
          "content": [{
              "type": "tool_result",
              "tool_use_id": "toolu_123",
              "content": "ok",
          }],
      },
      "parent_tool_use_id": None,
      "session_id": "session-id",
      "uuid": "event-id",
      "tool_use_result": {
          "stdout": "ok"
      },
  }


def test_result_event_uses_aggregate_usage_from_unique_messages() -> None:
  state = claude_sub.TurnState()
  state.observe_assistant(
      {
          "id": "msg-a",
          "content": [{
              "type": "text",
              "text": "final",
          }],
          "stop_reason": "end_turn",
          "usage":
              {
                  "input_tokens": 10,
                  "output_tokens": 2,
                  "cache_read_input_tokens": 3,
                  "cache_creation_input_tokens": 4,
              },
      })

  event = claude_sub._result_event("session-id", state, {"durationMs": 123})

  assert event["type"] == "result"
  assert event["subtype"] == "success"
  assert event["session_id"] == "session-id"
  assert event["result"] == "final"
  assert event["usage"]["input_tokens"] == 10
  assert event["usage"]["output_tokens"] == 2
