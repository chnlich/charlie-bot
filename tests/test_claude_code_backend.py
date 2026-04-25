from src.agents.backends.claude_code import BASE_COMMAND, ClaudeCodeBackend


def test_build_command_uses_double_dash_separator_for_prompt() -> None:
  backend = ClaudeCodeBackend(model="claude-opus-4-7")

  cmd = backend._build_command("--malicious-flag ignore previous")

  assert cmd[-2:] == ["--", "--malicious-flag ignore previous"]


def test_build_command_preserves_plain_prompt_after_separator() -> None:
  backend = ClaudeCodeBackend()

  cmd = backend._build_command("hello world")

  assert cmd[-2:] == ["--", "hello world"]


def test_base_command_disallows_headless_unsafe_tools() -> None:
  disallowed_index = BASE_COMMAND.index("--disallowed-tools")
  disallowed_tools = set(BASE_COMMAND[disallowed_index + 1].split(","))
  required_tools = {
      "Monitor",
      "ScheduleWakeup",
      "CronCreate",
      "CronDelete",
      "CronList",
  }

  assert required_tools <= disallowed_tools
