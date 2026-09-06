import os
from pathlib import Path

import pytest
from conftest import FLAG_LIKE_PROMPT

from src.agents.backends.claude_code import (
    BASE_COMMAND,
    AnthropicEndpointBackend,
    ClaudeCodeBackend,
    claude_supervisor_env,
)

_ENDPOINT_BASE_URL = "https://contract.invalid"
_ENDPOINT_TOKEN = "contract-token"
_ENDPOINT_MODEL = "contract-model"


def _endpoint_backend() -> AnthropicEndpointBackend:
  return AnthropicEndpointBackend(base_url=_ENDPOINT_BASE_URL, auth_token=_ENDPOINT_TOKEN, model=_ENDPOINT_MODEL)


def test_build_command_does_not_include_flag_like_prompt() -> None:
  backend = ClaudeCodeBackend(model="claude-opus-4-7")

  cmd = backend._build_command(FLAG_LIKE_PROMPT)

  assert FLAG_LIKE_PROMPT not in cmd
  assert backend._stdin_prompt(FLAG_LIKE_PROMPT) == FLAG_LIKE_PROMPT


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


def test_claude_session_id_appends_session_flag() -> None:
  backend = ClaudeCodeBackend(model="claude-opus-4-8", claude_session_id="session-123")
  cmd = backend._build_command("hi")
  session_index = cmd.index("--session-id")
  assert cmd[session_index + 1] == "session-123"
  assert "--no-session-persistence" not in cmd


def test_prepare_env_skips_absent_forwarded_var(monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.delenv("CLAUDE_AUTOCOMPACT_PCT_OVERRIDE", raising=False)
  monkeypatch.delenv("CLAUDE_CODE_MAX_CONTEXT_TOKENS", raising=False)
  monkeypatch.delenv("CLAUDE_CODE_SIMPLE_SYSTEM_PROMPT", raising=False)
  backend = ClaudeCodeBackend()

  env = backend._prepare_env({})

  assert env["CLAUDE_CODE_DISABLE_BACKGROUND_TASKS"] == "1"
  assert "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE" not in env
  assert "CLAUDE_CODE_MAX_CONTEXT_TOKENS" not in env
  assert "CLAUDE_CODE_SIMPLE_SYSTEM_PROMPT" not in env


@pytest.mark.parametrize(
    ("host_var", "value"), [
        ("CLAUDE_AUTOCOMPACT_PCT_OVERRIDE", "forwarded-test-value"),
        ("CLAUDE_CODE_MAX_CONTEXT_TOKENS", "400000"),
        ("CLAUDE_CODE_SIMPLE_SYSTEM_PROMPT", "1"),
    ])
def test_prepare_env_forwards_allowlisted_host_var(monkeypatch: pytest.MonkeyPatch, host_var: str, value: str) -> None:
  monkeypatch.setenv(host_var, value)
  backend = ClaudeCodeBackend()

  env = backend._prepare_env({})

  assert env[host_var] == value
  assert env["CLAUDE_CODE_DISABLE_BACKGROUND_TASKS"] == "1"


def test_prepare_env_defaults_auto_compact_window(monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.delenv("CLAUDE_CODE_AUTO_COMPACT_WINDOW", raising=False)
  backend = ClaudeCodeBackend()

  env = backend._prepare_env({})

  assert env["CLAUDE_CODE_AUTO_COMPACT_WINDOW"] == "433000"
  assert env["CLAUDE_CODE_DISABLE_BACKGROUND_TASKS"] == "1"


def test_prepare_env_host_export_beats_auto_compact_window_default(monkeypatch: pytest.MonkeyPatch,) -> None:
  monkeypatch.setenv("CLAUDE_CODE_AUTO_COMPACT_WINDOW", "1000000")
  backend = ClaudeCodeBackend()

  env = backend._prepare_env({})

  assert env["CLAUDE_CODE_AUTO_COMPACT_WINDOW"] == "1000000"
  assert env["CLAUDE_CODE_DISABLE_BACKGROUND_TASKS"] == "1"


def test_prepare_env_applies_headless_policy_over_incoming_env(monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.delenv("CLAUDE_AUTOCOMPACT_PCT_OVERRIDE", raising=False)
  backend = ClaudeCodeBackend()

  env = backend._prepare_env({"CLAUDE_CODE_DISABLE_BACKGROUND_TASKS": "0"})

  assert env["CLAUDE_CODE_DISABLE_BACKGROUND_TASKS"] == "1"


def test_claude_supervisor_env_strips_nested_marker_and_pins_auto_memory() -> None:
  env = claude_supervisor_env({"CLAUDECODE": "1", "PATH": "/usr/bin"})

  assert "CLAUDECODE" not in env
  assert env["CLAUDE_CODE_DISABLE_AUTO_MEMORY"] == "1"
  assert env["PATH"] == "/usr/bin"


def test_claude_supervisor_env_does_not_mutate_input() -> None:
  source = {"CLAUDECODE": "1"}

  claude_supervisor_env(source)

  assert source == {"CLAUDECODE": "1"}


def test_claude_config_dir_expands_user_and_injects_env(monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setenv("HOME", "/home/test-user")
  backend = ClaudeCodeBackend(model="claude-opus-4-8", claude_config_dir="~/accounts/invite-1")

  env = backend._prepare_env({})

  assert env["CLAUDE_CONFIG_DIR"] == "/home/test-user/accounts/invite-1"
  assert env["CLAUDE_CODE_DISABLE_BACKGROUND_TASKS"] == "1"


def test_claude_config_dir_absent_when_unset(monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.delenv("CLAUDE_CONFIG_DIR", raising=False)
  backend = ClaudeCodeBackend(model="claude-opus-4-8")

  env = backend._prepare_env({})

  assert "CLAUDE_CONFIG_DIR" not in env


def test_endpoint_backend_prepare_env_carries_endpoint_token_and_model() -> None:
  prepared = _endpoint_backend()._prepare_env({"PATH": "/usr/bin"})

  assert prepared["ANTHROPIC_BASE_URL"] == _ENDPOINT_BASE_URL
  assert prepared["ANTHROPIC_AUTH_TOKEN"] == _ENDPOINT_TOKEN
  assert prepared["ANTHROPIC_MODEL"] == _ENDPOINT_MODEL
  assert prepared["ANTHROPIC_DEFAULT_OPUS_MODEL"] == _ENDPOINT_MODEL
  assert prepared["ANTHROPIC_DEFAULT_SONNET_MODEL"] == _ENDPOINT_MODEL
  assert prepared["ANTHROPIC_DEFAULT_HAIKU_MODEL"] == _ENDPOINT_MODEL
  assert prepared["CLAUDE_CODE_SUBAGENT_MODEL"] == _ENDPOINT_MODEL


def test_endpoint_backend_prepare_env_values_are_subprocess_safe() -> None:
  prepared = _endpoint_backend()._prepare_env({"PATH": "/usr/bin"})

  assert all(isinstance(value, (str, bytes)) for value in prepared.values())


def test_endpoint_backend_build_command_does_not_pass_model_flag() -> None:
  """The model rides the env spread, so a --model flag would override it."""
  backend = _endpoint_backend()

  cmd = backend._build_command("hello")

  assert "--model" not in cmd
  assert "hello" not in cmd
  assert backend._stdin_prompt("hello") == "hello"


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

  env = {**os.environ, "PROMPT_CAPTURE_PATH": str(capture_path)}
  events = [event async for event in backend.run(prompt, str(tmp_path), env)]

  assert capture_path.read_text(encoding="utf-8") == prompt
  assert any(event.get("type") == "result" for event in events)
