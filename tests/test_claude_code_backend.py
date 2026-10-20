import json
import os
import pathlib

import conftest
import pytest

from src.backends.claude_code import claude_code, claude_config, claude_launch
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
      "ShareOnboardingGuide",
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
  """A cc-claude entry's login dir rides the pool account (claude_config.ClaudeAccount.config_dir);
  the backend expands ``~`` against HOME before injecting CLAUDE_CONFIG_DIR."""
  monkeypatch.setenv("HOME", "/home/test-user")
  option = conftest.backend_option(id="cc", label="CC", type="cc-claude", model="claude-opus-4-8")
  account = claude_config.ClaudeAccount(label="invite-1", config_dir="~/accounts/invite-1")

  backend = backend_types.build_backend(option, config.CharlieBotConfig(), claude_account=account)

  env = backend._prepare_env({})

  assert env["CLAUDE_CONFIG_DIR"] == "/home/test-user/accounts/invite-1"
  assert env["CLAUDE_CODE_DISABLE_BACKGROUND_TASKS"] == "1"
  assert env["BASH_DEFAULT_TIMEOUT_MS"] == "900000"
  assert env["BASH_MAX_TIMEOUT_MS"] == "900000"


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


# ---------------------------------------------------------------------------
# Per-run-kind launch settings: the declared window, the tool set and the skill
# listing each run kind launches with
# ---------------------------------------------------------------------------

_WINDOW_ENV_NAMES = (
    claude_launch.AUTO_COMPACT_WINDOW_ENV,
    claude_launch.AUTOCOMPACT_PCT_OVERRIDE_ENV,
    claude_launch.MAX_CONTEXT_TOKENS_ENV,
)


@pytest.fixture
def _clean_window_env(monkeypatch: pytest.MonkeyPatch) -> None:
  """A host that exports none of the window variables, so the kind's own value is the one read."""
  for name in _WINDOW_ENV_NAMES:
    monkeypatch.delenv(name, raising=False)


def _settings_of(cmd: list[str]) -> dict:
  """The one --settings object a Claude argv carries."""
  return json.loads(cmd[cmd.index("--settings") + 1])


@pytest.mark.usefixtures("_clean_window_env")
@pytest.mark.parametrize(
    ("run_kind", "window"), [
        pytest.param("manager_turn", "433000", id="manager-turn"),
        pytest.param("work", "433000", id="work"),
        pytest.param("review", "433000", id="review"),
        pytest.param("iteration", "433000", id="iteration"),
        pytest.param("scheduled_step", "433000", id="scheduled-step"),
    ])
def test_a_claude_run_declares_its_kinds_window_and_disables_the_advisor(run_kind: models.RunKind, window: str) -> None:
  backend = claude_code.ClaudeCodeBackend(run_kind=run_kind)

  env = backend._prepare_env({})

  assert env[claude_launch.AUTO_COMPACT_WINDOW_ENV] == window
  assert env["CLAUDE_CODE_DISABLE_ADVISOR_TOOL"] == "1"


@pytest.mark.parametrize(
    ("run_kind", "report_findings"), [
        pytest.param("manager_turn", False, id="manager-turn"),
        pytest.param("work", False, id="work"),
        pytest.param("review", True, id="review"),
        pytest.param("iteration", False, id="iteration"),
        pytest.param("scheduled_step", False, id="scheduled-step"),
    ])
def test_only_a_review_run_keeps_report_findings(run_kind: models.RunKind, report_findings: bool) -> None:
  backend = claude_code.ClaudeCodeBackend(run_kind=run_kind)

  tools = _disallowed_tool_values(backend._build_command("hi"))

  assert "ShareOnboardingGuide" in tools
  assert ("ReportFindings" not in tools) is report_findings


def test_a_host_exported_window_beats_the_kinds_value(monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setenv(claude_launch.AUTO_COMPACT_WINDOW_ENV, "500000")

  for run_kind in ("manager_turn", "work", "review"):
    backend = claude_code.ClaudeCodeBackend(run_kind=run_kind)
    assert backend._prepare_env({})[claude_launch.AUTO_COMPACT_WINDOW_ENV] == "500000"


def test_name_only_skills_ride_the_one_merged_settings_object() -> None:
  backend = claude_code.ClaudeCodeBackend(run_kind="review", name_only_skills=("synthetic-alpha", "synthetic-beta"))

  settings = _settings_of(backend._build_command("hi"))

  assert settings == {
      "disableClaudeAiConnectors": True,
      "skillOverrides": {
          "synthetic-alpha": "name-only",
          "synthetic-beta": "name-only"
      },
  }
  assert backend._build_command("hi").count("--settings") == 1


def test_a_kind_with_no_name_only_list_keeps_the_settings_object_as_before() -> None:
  backend = claude_code.ClaudeCodeBackend(run_kind="review")

  assert _settings_of(backend._build_command("hi")) == {"disableClaudeAiConnectors": True}
