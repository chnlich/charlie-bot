import conftest
import pytest

from src.backends.gemini import gemini_cli
from src.infra import event_types as ET


def _build_backend(monkeypatch: pytest.MonkeyPatch, **kwargs: object) -> gemini_cli.GeminiCliBackend:
  return conftest.build_cli_backend_rig(monkeypatch, gemini_cli.GeminiCliBackend, **kwargs)


def test_build_command_wraps_instructions_and_resume(monkeypatch: pytest.MonkeyPatch) -> None:
  backend = _build_backend(
      monkeypatch,
      model="gemini-test-model",
      instructions_content="Use concise answers.",
      resume_session_id="session-123",
      extra_flags=["--approval-mode", "yolo"],
  )

  cmd = backend._build_command("Hello")
  expected_prompt = "<system-instructions>\nUse concise answers.\n</system-instructions>\n\nHello"
  assert cmd == [
      "/usr/bin/gemini",
      "-m",
      "gemini-test-model",
      "-p",
      expected_prompt,
      "-o",
      "stream-json",
      "-y",
      "--sandbox=false",
      "--resume",
      "session-123",
      "--approval-mode",
      "yolo",
  ]


def test_build_command_preserves_dash_prefixed_prompt(monkeypatch: pytest.MonkeyPatch) -> None:
  backend = _build_backend(monkeypatch, model="gemini-test-model")

  cmd = backend._build_command("--watch-pid only local")

  assert cmd == [
      "/usr/bin/gemini",
      "-m",
      "gemini-test-model",
      "-p",
      "--watch-pid only local",
      "-o",
      "stream-json",
      "-y",
      "--sandbox=false",
  ]


def test_prepare_env_strips_api_keys(monkeypatch: pytest.MonkeyPatch) -> None:
  backend = _build_backend(monkeypatch)
  original_env = {
      "PATH": "/usr/bin",
      "GEMINI_API_KEY": "gemini-key",
      "GOOGLE_API_KEY": "google-key",
  }

  prepared = backend._prepare_env(original_env)

  assert "GEMINI_API_KEY" not in prepared
  assert "GOOGLE_API_KEY" not in prepared
  assert prepared["PATH"] == "/usr/bin"


def test_translate_event_mappings(monkeypatch: pytest.MonkeyPatch) -> None:
  backend = _build_backend(monkeypatch)

  assert backend.translate_event({
      "type": "init",
      "session_id": "sid"
  }) == [{
      "type": ET.SESSION_ATTACHED,
      "session_id": "sid"
  }]
  assert not backend.translate_event({"type": "message", "role": "user", "content": "ignored"})
  assert backend.translate_event({
      "type": "message",
      "role": "assistant",
      "content": "hello"
  }) == [conftest.assistant_text_event("hello")]
  assert backend.translate_event({
      "type": "tool_use",
      "tool_name": "Bash",
      "parameters": {
          "command": "pwd"
      },
  }) == [{
      "type": "tool_use",
      "name": "Bash",
      "input": {
          "command": "pwd"
      },
  }]
  assert backend.translate_event({
      "type": "tool_result",
      "status": "success",
      "tool_id": "Bash",
      "output": "/tmp",
  }) == [{
      "type": "tool_result",
      "tool_name": "Bash",
      "content": "/tmp",
  }]
  assert backend.translate_event({
      "type": "error",
      "message": "boom"
  }) == [{
      "type": "error",
      "message": "boom",
      "content": "boom",
  }]
  assert backend.translate_event({
      "type": "result",
      "stats": {
          "input_tokens": 10,
          "output_tokens": 5,
          "cached": 3,
      },
  }) == [
      {
          "type": "result",
          "result": "",
          "usage":
              {
                  "input_tokens": 10,
                  "output_tokens": 5,
                  "cache_read_input_tokens": 3,
                  "cache_creation_input_tokens": 0,
              },
          "total_cost_usd": 0,
      }
  ]
