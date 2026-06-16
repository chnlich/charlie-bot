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


def _disallowed_tool_values(cmd: list[str]) -> set[str]:
  tools: set[str] = set()
  for i, token in enumerate(cmd):
    if token == "--disallowed-tools":
      tools.update(cmd[i + 1].split(","))
  return tools


def test_subscription_backend_disallows_interactive_menu_tools() -> None:
  backend = ClaudeCodeBackend(model="claude-opus-4-8", cli_binary="claude-sub")

  tools = _disallowed_tool_values(backend._build_command("hi"))

  assert {"AskUserQuestion", "ExitPlanMode"} <= tools
  assert "Monitor" in tools  # base headless-unsafe tools still disallowed


def test_api_backend_does_not_disallow_interactive_menu_tools() -> None:
  backend = ClaudeCodeBackend(model="claude-opus-4-8")

  tools = _disallowed_tool_values(backend._build_command("hi"))

  assert "AskUserQuestion" not in tools
  assert "ExitPlanMode" not in tools
  assert "Monitor" in tools


def test_fast_mode_appends_settings_flag() -> None:
  backend = ClaudeCodeBackend(model="claude-opus-4-8", fast_mode=True)
  cmd = backend._build_command("hi")
  settings_index = cmd.index("--settings")
  assert cmd[settings_index + 1] == '{"fastMode":true}'


def test_fast_mode_absent_when_unset() -> None:
  backend = ClaudeCodeBackend(model="claude-opus-4-8")
  cmd = backend._build_command("hi")
  assert "--settings" not in cmd


def test_translate_event_passes_through_non_assistant() -> None:
  backend = ClaudeCodeBackend()
  event = {"type": "system", "subtype": "api_retry"}

  assert backend.translate_event(event) == [event]


def test_translate_event_extracts_thinking_blocks() -> None:
  backend = ClaudeCodeBackend()
  event = {
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
                      {
                          "type": "thinking",
                          "thinking": "Step two",
                          "signature": "sig2"
                      },
                  ]
          },
  }

  assert backend.translate_event(event) == [
      event,
      {
          "type": "thinking",
          "content": "Step one"
      },
      {
          "type": "thinking",
          "content": "Step two"
      },
  ]


def test_translate_event_ignores_empty_thinking_blocks() -> None:
  backend = ClaudeCodeBackend()
  event = {
      "type": "assistant",
      "message":
          {
              "content":
                  [
                      {
                          "type": "thinking",
                          "thinking": "",
                          "signature": "sig1"
                      },
                      {
                          "type": "thinking",
                          "signature": "sig2"
                      },
                  ]
          },
  }

  assert backend.translate_event(event) == [event]


def test_translate_event_tolerates_non_list_content() -> None:
  backend = ClaudeCodeBackend()
  event = {"type": "assistant", "message": {"content": "plain text"}}

  assert backend.translate_event(event) == [event]
