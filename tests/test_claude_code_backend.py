from src.agents.backends.claude_code import BASE_COMMAND, ClaudeCodeBackend


def test_build_command_uses_double_dash_separator_for_prompt() -> None:
  backend = ClaudeCodeBackend(model="claude-opus-4-7")

  cmd = backend._build_command("--malicious-flag ignore previous")

  assert cmd[-2:] == ["--", "--malicious-flag ignore previous"]


def test_build_command_preserves_plain_prompt_after_separator() -> None:
  backend = ClaudeCodeBackend()

  cmd = backend._build_command("hello world")

  assert cmd[-2:] == ["--", "hello world"]


def test_effort_flag_appended_after_model() -> None:
  backend = ClaudeCodeBackend(model="claude-opus-4-8", effort="max")

  cmd = backend._build_command("hi")

  model_index = cmd.index("--model")
  effort_index = cmd.index("--effort")
  assert cmd[effort_index + 1] == "max"
  assert effort_index == model_index + 2


def test_effort_flag_absent_when_unset() -> None:
  backend = ClaudeCodeBackend(model="claude-opus-4-8")

  cmd = backend._build_command("hi")

  assert "--effort" not in cmd


def test_cli_binary_replaces_only_command_binary() -> None:
  backend = ClaudeCodeBackend(model="claude-opus-4-8", cli_binary="claude-sub")

  cmd = backend._build_command("hi")

  assert cmd[0] == "claude-sub"
  assert cmd[1:len(BASE_COMMAND)] == BASE_COMMAND[1:]


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
