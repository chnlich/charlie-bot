import os
import pathlib

import conftest
import pytest

from src.backends.claude_code import claude_code
from src.infra import config, models
from src.runtime.hooks import backend_types


def test_build_command_sends_plain_prompt_via_stdin_hook() -> None:
  backend = claude_code.ClaudeCodeBackend()

  prompt = "hello world"
  cmd = backend._build_command(prompt)

  assert prompt not in cmd
  assert backend._stdin_prompt(prompt) == prompt


def test_base_command_disallows_headless_unsafe_tools() -> None:
  disallowed_index = claude_code.BASE_COMMAND.index("--disallowed-tools")
  disallowed_tools = set(claude_code.BASE_COMMAND[disallowed_index + 1].split(","))
  required_tools = {
      "Monitor",
      "ScheduleWakeup",
      "CronCreate",
      "CronDelete",
      "CronList",
      "Agent",
      "Workflow",
      "TaskCreate",
      "TaskGet",
      "TaskUpdate",
      "TaskList",
      "TaskStop",
      "TaskOutput",
      "SendMessage",
      "ListAgents",
  }

  assert required_tools <= disallowed_tools


def _disallowed_tool_values(cmd: list[str]) -> set[str]:
  tools: set[str] = set()
  for i, token in enumerate(cmd):
    if token == "--disallowed-tools":
      tools.update(cmd[i + 1].split(","))
  return tools


def test_api_backend_does_not_disallow_interactive_menu_tools() -> None:
  backend = claude_code.ClaudeCodeBackend(model="claude-opus-4-8")

  tools = _disallowed_tool_values(backend._build_command("hi"))

  assert "AskUserQuestion" not in tools
  assert "ExitPlanMode" not in tools
  assert "Monitor" in tools


def test_claude_supervisor_env_does_not_mutate_input() -> None:
  source = {"CLAUDECODE": "1"}

  claude_code.claude_supervisor_env(source)

  assert source == {"CLAUDECODE": "1"}


def test_pool_account_config_dir_expands_user_and_injects_env(monkeypatch: pytest.MonkeyPatch) -> None:
  """A cc-claude entry's login dir rides the pool account (ClaudeAccount.config_dir);
  the backend expands ``~`` against HOME before injecting CLAUDE_CONFIG_DIR."""
  monkeypatch.setenv("HOME", "/home/test-user")
  option = conftest.backend_option(id="cc", label="CC", type="cc-claude", model="claude-opus-4-8")
  account = models.ClaudeAccount(label="invite-1", config_dir="~/accounts/invite-1")

  backend = backend_types.build_backend(option, config.CharlieBotConfig(), claude_account=account)

  env = backend._prepare_env({})

  assert env["CLAUDE_CONFIG_DIR"] == "/home/test-user/accounts/invite-1"
  assert env["CLAUDE_CODE_DISABLE_BACKGROUND_TASKS"] == "1"


@pytest.mark.asyncio
async def test_large_prompt_is_sent_on_stdin_not_argv(tmp_path: pathlib.Path) -> None:
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
  backend = claude_code.ClaudeCodeBackend(cli_binary=str(stub))

  env = {**os.environ, "PROMPT_CAPTURE_PATH": str(capture_path)}
  events = [event async for event in backend.run(prompt, str(tmp_path), env)]

  assert capture_path.read_text(encoding="utf-8") == prompt
  assert any(event.get("type") == "result" for event in events)
