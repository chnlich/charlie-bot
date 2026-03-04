from src.agents.backends.gemini_cli import GeminiCliBackend


def _build_backend(monkeypatch, **kwargs) -> GeminiCliBackend:
  monkeypatch.setattr(
      "src.agents.backends.gemini_cli._resolve_gemini_binary",
      lambda: "/usr/bin/gemini",
  )
  return GeminiCliBackend(**kwargs)


def test_build_command_wraps_instructions_and_resume(monkeypatch) -> None:
  backend = _build_backend(
      monkeypatch,
      model="gemini-3-pro-preview",
      instructions_content="Use concise answers.",
      resume_session_id="session-123",
      extra_flags=["--approval-mode", "yolo"],
  )

  cmd = backend._build_command("Hello")
  expected_prompt = "<system-instructions>\nUse concise answers.\n</system-instructions>\n\nHello"
  assert cmd == [
      "/usr/bin/gemini",
      "-m",
      "gemini-3-pro-preview",
      "-p",
      expected_prompt,
      "-o",
      "stream-json",
      "-y",
      "--resume",
      "session-123",
      "--approval-mode",
      "yolo",
  ]


def test_prepare_env_preserves_api_keys(monkeypatch) -> None:
  backend = _build_backend(monkeypatch)
  original_env = {
      "PATH": "/usr/bin",
      "GEMINI_API_KEY": "gemini-key",
      "GOOGLE_API_KEY": "google-key",
  }

  prepared = backend._prepare_env(original_env)

  assert prepared["GEMINI_API_KEY"] == "gemini-key"
  assert prepared["GOOGLE_API_KEY"] == "google-key"
  assert prepared["PATH"] == "/usr/bin"


def test_translate_event_mappings(monkeypatch) -> None:
  backend = _build_backend(monkeypatch)

  assert backend.translate_event({"type": "init", "session_id": "sid"}) == [{"session_id": "sid"}]
  assert backend.translate_event({"type": "message", "role": "user", "content": "ignored"}) == []
  assert backend.translate_event({"type": "message", "role": "assistant", "content": "hello"}) == [{
      "type": "assistant",
      "message": {"content": [{"type": "text", "text": "hello"}]},
  }]
  assert backend.translate_event({
      "type": "tool_use",
      "tool_name": "Bash",
      "parameters": {"command": "pwd"},
  }) == [{
      "type": "tool_use",
      "name": "Bash",
      "input": {"command": "pwd"},
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
  assert backend.translate_event({"type": "error", "message": "boom"}) == [{
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
  }) == [{
      "type": "result",
      "result": "",
      "usage": {
          "input_tokens": 10,
          "output_tokens": 5,
          "cache_read_input_tokens": 3,
          "cache_creation_input_tokens": 0,
      },
      "total_cost_usd": 0,
  }]


def test_translate_event_tool_result_error_and_unknown(monkeypatch) -> None:
  backend = _build_backend(monkeypatch)

  assert backend.translate_event({
      "type": "tool_result",
      "status": "error",
      "tool_id": "Bash",
      "error": {"message": "permission denied"},
  }) == [{
      "type": "tool_result",
      "tool_name": "Bash",
      "content": "permission denied",
  }]
  assert backend.translate_event({
      "type": "tool_result",
      "status": "error",
      "tool_id": "Bash",
      "error": "generic failure",
  }) == [{
      "type": "tool_result",
      "tool_name": "Bash",
      "content": "generic failure",
  }]
  assert backend.translate_event({"type": "unhandled"}) == []
