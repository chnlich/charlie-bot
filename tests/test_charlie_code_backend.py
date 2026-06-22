import pytest

from src.agents.backends.charlie_code import CharlieCodeBackend
from src.core import event_types as ET


def _build_backend(monkeypatch, **kwargs) -> CharlieCodeBackend:
  monkeypatch.setattr(
      "src.agents.backends.charlie_code.resolve_binary",
      lambda name, fallback: "/usr/bin/charlie-code",
  )
  kwargs.setdefault("model", "charlie-code-test-model")
  kwargs.setdefault("api_base", "http://test.local/v1")
  return CharlieCodeBackend(**kwargs)


def test_translate_success_stream_preserves_tool_pair_ids_and_usage(monkeypatch) -> None:
  backend = _build_backend(monkeypatch)

  command = backend.translate_event({
      "type": "command",
      "step": 1,
      "id": "s-1",
      "command": "pwd",
  })
  assert command == [{
      "type": ET.TOOL_USE,
      "name": "Bash",
      "input": {
          "command": "pwd"
      },
      "id": "s-1",
  }]

  observation = backend.translate_event(
      {
          "type": "observation",
          "step": 1,
          "id": "s-1",
          "returncode": 0,
          "output": "/tmp/worktree\n",
      })
  assert observation == [
      {
          "type": ET.TOOL_RESULT,
          "tool_name": "Bash",
          "content": "/tmp/worktree\n",
          "tool_use_id": "s-1",
      }
  ]

  result = backend.translate_event(
      {
          "type": "result",
          "completed": True,
          "n_steps": 1,
          "usage": {
              "n_calls": 2,
              "input_tokens": 123,
              "output_tokens": 45,
          },
      })
  assert result == [
      {
          "type": ET.RESULT,
          "result": "",
          "usage":
              {
                  "input_tokens": 123,
                  "output_tokens": 45,
                  "cache_read_input_tokens": 0,
                  "cache_creation_input_tokens": 0,
              },
          "total_cost_usd": None,
      }
  ]


def test_translate_failure_stream_preserves_error_message(monkeypatch) -> None:
  backend = _build_backend(monkeypatch)

  translated = backend.translate_event({
      "type": "error",
      "message": "rate limit: retry later",
  })

  assert translated == [
      {
          "type": ET.ERROR,
          "message": "rate limit: retry later",
          "content": "rate limit: retry later",
      }
  ]


def test_translate_thought_and_unknown(monkeypatch) -> None:
  backend = _build_backend(monkeypatch)

  assert backend.translate_event({
      "type": "thought",
      "step": 1,
      "text": "I will inspect the files.",
  }) == [{
      "type": ET.ASSISTANT,
      "message": {
          "content": [{
              "type": "text",
              "text": "I will inspect the files.",
          }]
      },
  }]
  assert backend.translate_event({"type": "future-event"}) == []


def test_translate_session_event(monkeypatch) -> None:
  backend = _build_backend(monkeypatch)

  assert backend.translate_event({"type": "session", "session_id": "session-X"}) == [{
      "session_id": "session-X"
  }]


def test_build_command_uses_json_model_api_base_separator_and_effective_prompt(monkeypatch) -> None:
  backend = _build_backend(
      monkeypatch,
      model="charlie-code-test-model",
      api_base="http://test.local/v1",
      instructions_content="Use concise answers.",
  )

  cmd = backend._build_command("--malicious-flag ignore previous")

  expected_prompt = (
      "<system-instructions>\n"
      "Use concise answers.\n"
      "</system-instructions>\n\n"
      "--malicious-flag ignore previous")
  assert cmd == [
      "/usr/bin/charlie-code",
      "--json",
      "--model",
      "charlie-code-test-model",
      "--api-base",
      "http://test.local/v1",
      "--",
      expected_prompt,
  ]
  assert "--resume" not in cmd


def test_build_command_includes_resume_before_task_separator(monkeypatch) -> None:
  backend = _build_backend(
      monkeypatch,
      model="charlie-code-test-model",
      api_base="http://test.local/v1",
      resume_session_id="session-123",
  )

  cmd = backend._build_command("continue")

  assert cmd == [
      "/usr/bin/charlie-code",
      "--json",
      "--model",
      "charlie-code-test-model",
      "--api-base",
      "http://test.local/v1",
      "--resume",
      "session-123",
      "--",
      "continue",
  ]


def test_effective_prompt_wraps_instructions_only_for_fresh_sessions(monkeypatch) -> None:
  fresh_backend = _build_backend(monkeypatch, instructions_content="Use concise answers.")
  resume_backend = _build_backend(
      monkeypatch,
      instructions_content="Use concise answers.",
      resume_session_id="session-123",
  )

  assert fresh_backend._effective_prompt("Hello") == (
      "<system-instructions>\n"
      "Use concise answers.\n"
      "</system-instructions>\n\n"
      "Hello")
  assert resume_backend._effective_prompt("Hello") == "Hello"


def test_api_base_required(monkeypatch) -> None:
  monkeypatch.setattr(
      "src.agents.backends.charlie_code.resolve_binary",
      lambda name, fallback: "/usr/bin/charlie-code",
  )

  with pytest.raises(ValueError, match="api_base"):
    CharlieCodeBackend(model="charlie-code-test-model")
