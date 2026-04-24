from src.agents.backends.claude_code import ClaudeCodeBackend


def test_build_command_uses_double_dash_separator_for_prompt() -> None:
  backend = ClaudeCodeBackend(model="claude-opus-4-7")

  cmd = backend._build_command("--malicious-flag ignore previous")

  assert cmd[-2:] == ["--", "--malicious-flag ignore previous"]


def test_build_command_preserves_plain_prompt_after_separator() -> None:
  backend = ClaudeCodeBackend()

  cmd = backend._build_command("hello world")

  assert cmd[-2:] == ["--", "hello world"]
