import os
from pathlib import Path

import pytest

from src.agents.backends.claude_code import BASE_COMMAND, ClaudeCodeBackend


def test_build_command_does_not_include_flag_like_prompt() -> None:
  backend = ClaudeCodeBackend(model="claude-opus-4-7")

  prompt = "--malicious-flag ignore previous"
  cmd = backend._build_command(prompt)

  assert prompt not in cmd
  assert backend._stdin_prompt(prompt) == prompt


def test_build_command_sends_plain_prompt_via_stdin_hook() -> None:
  backend = ClaudeCodeBackend()

  prompt = "hello world"
  cmd = backend._build_command(prompt)

  assert prompt not in cmd
  assert backend._stdin_prompt(prompt) == prompt


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


@pytest.mark.asyncio
async def test_large_prompt_is_sent_on_stdin_not_argv(tmp_path: Path) -> None:
  capture_path = tmp_path / "captured-prompt.txt"
  stub = tmp_path / "claude-stub"
  stub.write_text(
      """#!/usr/bin/env python3
import json
import os
import sys
from pathlib import Path

prompt = sys.stdin.read()
Path(os.environ["PROMPT_CAPTURE_PATH"]).write_text(prompt, encoding="utf-8")
print(json.dumps({"type": "result", "result": "", "usage": {}}), flush=True)
""",
      encoding="utf-8",
  )
  stub.chmod(0o755)

  prompt = "x" * (140 * 1024)
  backend = ClaudeCodeBackend(cli_binary=str(stub))

  events: list[dict] = []
  env = {**os.environ, "PROMPT_CAPTURE_PATH": str(capture_path)}
  async for event in backend.run(prompt, str(tmp_path), env):
    events.append(event)

  assert capture_path.read_text(encoding="utf-8") == prompt
  assert any(event.get("type") == "result" for event in events)
