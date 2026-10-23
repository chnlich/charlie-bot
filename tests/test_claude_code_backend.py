import asyncio
import json
import os
import pathlib

import conftest
import pytest

from src.backends.claude_code import claude_code, claude_config, claude_launch
from src.infra import config, models
from src.infra import event_types as ET
from src.runtime import midturn_delivery
from src.runtime.agent_process import base
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


def test_prepare_env_strips_an_inherited_cc_anthropic_compatible_worker_env(monkeypatch: pytest.MonkeyPatch) -> None:
  """A cc-claude run inside a cc-anthropic-compatible worker inherits the worker's gateway
  variables; the prepared run environment must drop them so `claude` talks to the Claude
  account, not the gateway."""
  for name in claude_code.GATEWAY_ENDPOINT_ENV_NAMES:
    monkeypatch.setenv(name, "inherited-gateway-value")
  inherited_env = {name: os.environ[name] for name in claude_code.GATEWAY_ENDPOINT_ENV_NAMES}
  inherited_env.update({"PATH": "/usr/bin", "UNRELATED": "kept"})

  env = claude_code.ClaudeCodeBackend(model="claude-opus-4-8")._prepare_env(inherited_env)

  for name in claude_code.GATEWAY_ENDPOINT_ENV_NAMES:
    assert name not in env
  assert env["PATH"] == "/usr/bin"
  assert env["UNRELATED"] == "kept"


@pytest.mark.asyncio
async def test_one_shot_text_spawns_claude_without_the_gateway_variables(monkeypatch: pytest.MonkeyPatch) -> None:
  for name in claude_code.GATEWAY_ENDPOINT_ENV_NAMES:
    monkeypatch.setenv(name, "inherited-gateway-value")
  monkeypatch.setenv("UNRELATED_INHERITED_VAR", "kept")

  spawned_env: dict[str, str] = {}

  class _FakeProc:

    returncode = 0

    async def communicate(self, input: bytes | None = None) -> tuple[bytes, bytes]:  # noqa: A002
      return b"ok", b""

  async def _fake_exec(*args: object, **kwargs: object) -> _FakeProc:
    spawned_env.update(kwargs["env"])
    return _FakeProc()

  monkeypatch.setattr(claude_code.asyncio, "create_subprocess_exec", _fake_exec)
  backend = claude_code.ClaudeCodeBackend(model="claude-haiku-5-5")

  assert await backend.one_shot_text("prompt", "system", timeout=None) == "ok"
  for name in claude_code.GATEWAY_ENDPOINT_ENV_NAMES:
    assert name not in spawned_env
  assert spawned_env["UNRELATED_INHERITED_VAR"] == "kept"


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


class _FakeStdin:

  def __init__(self) -> None:
    self.lines: list[bytes] = []
    self.closed = False

  def write(self, data: bytes) -> None:
    self.lines.append(data)

  async def drain(self) -> None:
    return None

  def is_closing(self) -> bool:
    return self.closed

  def close(self) -> None:
    self.closed = True

  async def wait_closed(self) -> None:
    return None


class _FakeProcess:

  def __init__(self) -> None:
    self.stdin = _FakeStdin()
    self.returncode = None


class _FakeBackend:

  def __init__(self, run_id: str) -> None:
    self._midturn_delivery = True
    self._midturn_run_id = run_id
    self._proc = _FakeProcess()


@pytest.mark.asyncio
async def test_midturn_offer_writes_only_web_user_messages_and_stops_at_result() -> None:
  run_id = "11111111-2222-3333-4444-555555555555"
  message_id = "22222222-2222-3333-4444-555555555555"
  backend = _FakeBackend(run_id)
  meta = models.SessionMetadata(profile="manager", id="session", name="Web")
  run = models.RunRecord(id=run_id, session_id=meta.id, kind="manager_turn")
  user_event = {"id": message_id, "type": ET.USER, "content": "Please change the answer."}
  agent_event = {"id": "agent-1", "type": ET.AGENT_MESSAGE, "content": "Keep this queued."}
  midturn_delivery.bind_process(run_id, backend)
  await midturn_delivery.write_initial_prompt(run_id, backend, "Initial prompt")

  try:
    assert await midturn_delivery.offer(meta, run, [user_event, agent_event]) == [message_id]
    encoded = [json.loads(line) for line in backend._proc.stdin.lines]
    assert [line["uuid"] for line in encoded] == [run_id, message_id]
    assert encoded[0]["message"]["content"] == "Initial prompt"
    assert encoded[1]["message"]["content"] == "Please change the answer."

    thread_meta = models.SessionMetadata(
        profile="manager",
        id="thread",
        name="Slack",
        slack_origin={
            "team_id": "T",
            "channel_id": "C",
            "thread_ts": "1.0"
        },
    )
    assert await midturn_delivery.offer(thread_meta, run, [user_event]) == []
    discord_meta = models.SessionMetadata(
        profile="manager",
        id="discord-thread",
        name="Discord",
        discord_origin={
            "guild_id": "G",
            "parent_channel_id": "C",
            "thread_id": "1"
        },
    )
    assert await midturn_delivery.offer(discord_meta, run, [user_event]) == []
    worker_run = models.RunRecord(id=run_id, session_id=meta.id, kind="work")
    assert await midturn_delivery.offer(meta, worker_run, [user_event]) == []

    await midturn_delivery.on_backend_event(run_id, {"type": ET.RESULT})
    assert backend._proc.stdin.is_closing()
    assert await midturn_delivery.offer(meta, run, [{**user_event, "id": "late-after-result"}]) == []
    assert len(backend._proc.stdin.lines) == 2
  finally:
    midturn_delivery.release_run(run_id)


def test_replay_becomes_non_rendered_input_delivered_event() -> None:
  run_id = "11111111-2222-3333-4444-555555555555"
  message_id = "22222222-2222-3333-4444-555555555555"
  assert midturn_delivery.translate_replay(run_id, {"type": ET.USER, "uuid": run_id, "isReplay": True}) == []

  delivered = midturn_delivery.translate_replay(run_id, {"type": ET.USER, "uuid": message_id, "isReplay": True})

  assert len(delivered) == 1
  assert delivered[0]["type"] == ET.INPUT_DELIVERED
  assert delivered[0]["input_event_id"] == message_id
  assert delivered[0]["run_id"] == run_id
  from src.runtime.api import message_utils
  assert message_utils.events_to_messages(delivered) == []


@pytest.mark.asyncio
@pytest.mark.integration
async def test_replay_after_result_resets_post_result_timeout(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A late replay keeps a same-process second turn alive across long silent tool work."""
  monkeypatch.setattr(base.AgentBackend, "_POST_RESULT_TIMEOUT", 0.1)
  monkeypatch.setattr(base.AgentBackend, "_CLEANUP_TIMEOUT", 2.0)
  run_id = "11111111-2222-3333-4444-555555555555"
  message_id = "22222222-2222-3333-4444-555555555555"
  marker = tmp_path / "waiting-for-input"
  stub = tmp_path / "claude-stream-json-stub"
  stub.write_text(
      """#!/usr/bin/env python3
import json
import os
import sys
import time

initial = json.loads(sys.stdin.readline())
open(os.environ["WAIT_MARKER"], "w", encoding="utf-8").close()
late = json.loads(sys.stdin.readline())
print(json.dumps({"type": "result", "result": "first", "usage": {}}), flush=True)
print(json.dumps({"type": "user", "uuid": late["uuid"], "isReplay": True,
                  "message": late["message"]}), flush=True)
time.sleep(0.35)
print(json.dumps({"type": "result", "result": "second", "usage": {}}), flush=True)
""",
      encoding="utf-8",
  )
  stub.chmod(0o755)
  backend = claude_code.ClaudeCodeBackend(
      cli_binary=str(stub),
      midturn_delivery=True,
      midturn_run_id=run_id,
      log_dir=tmp_path / "run",
  )
  midturn_delivery.bind_process(run_id, backend)
  meta = models.SessionMetadata(profile="manager", id="session", name="Web")
  run = models.RunRecord(id=run_id, session_id=meta.id, kind="manager_turn")
  event = {"id": message_id, "type": ET.USER, "content": "Continue the answer."}
  env = {**os.environ, "WAIT_MARKER": str(marker)}

  async def collect() -> list[dict]:
    return [item async for item in backend.run("Initial prompt", str(tmp_path), env)]

  task = asyncio.create_task(collect())
  try:
    deadline = asyncio.get_running_loop().time() + 5
    while not marker.exists() and asyncio.get_running_loop().time() < deadline:
      await asyncio.sleep(0.01)
    assert marker.exists(), "the fake CLI did not read the initial JSON user message"
    assert await midturn_delivery.offer(meta, run, [event]) == [message_id]
    events = await asyncio.wait_for(task, timeout=5)
    assert [item["result"] for item in events if item.get("type") == ET.RESULT] == ["first", "second"]
    assert any(item.get("type") == ET.INPUT_DELIVERED and item.get("input_event_id") == message_id for item in events)
    assert backend.exit_code == 0
    assert backend.hang_diagnostics is None
  finally:
    if not task.done():
      task.cancel()
      with pytest.raises(asyncio.CancelledError):
        await task
    midturn_delivery.release_run(run_id)


# ---------------------------------------------------------------------------
# Per-run-kind launch settings: the declared window, settings and tool set
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
  kwargs = {"cgroup_session_id": "manager-session"} if run_kind == "manager_turn" else {}
  backend = claude_code.ClaudeCodeBackend(run_kind=run_kind, **kwargs)

  env = backend._prepare_env({})

  assert env[claude_launch.AUTO_COMPACT_WINDOW_ENV] == window
  assert env["CLAUDE_CODE_DISABLE_ADVISOR_TOOL"] == "1"


def test_a_host_exported_window_beats_the_kinds_value(monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setenv(claude_launch.AUTO_COMPACT_WINDOW_ENV, "500000")

  for run_kind in ("manager_turn", "work", "review"):
    kwargs = {"cgroup_session_id": "manager-session"} if run_kind == "manager_turn" else {}
    backend = claude_code.ClaudeCodeBackend(run_kind=run_kind, **kwargs)
    assert backend._prepare_env({})[claude_launch.AUTO_COMPACT_WINDOW_ENV] == "500000"


@pytest.mark.parametrize(
    ("run_kind", "expected_settings", "skill_disallowed", "report_findings_disallowed"), [
        pytest.param(
            "work",
            {
                "disableClaudeAiConnectors": True,
                "syncClaudeAiSkills": False,
                "disableBundledSkills": True
            },
            False,
            True,
            id="work",
        ),
        pytest.param(
            "iteration",
            {
                "disableClaudeAiConnectors": True,
                "syncClaudeAiSkills": False,
                "disableBundledSkills": True
            },
            False,
            True,
            id="iteration",
        ),
        pytest.param(
            "scheduled_step",
            {
                "disableClaudeAiConnectors": True,
                "syncClaudeAiSkills": False,
                "disableBundledSkills": True
            },
            False,
            True,
            id="scheduled-step",
        ),
        pytest.param(
            "review",
            {
                "disableClaudeAiConnectors": True,
                "syncClaudeAiSkills": False,
                "disableBundledSkills": True
            },
            True,
            False,
            id="review",
        ),
    ])
def test_each_run_kind_gets_its_settings_and_disallowed_tools(
    run_kind: models.RunKind,
    expected_settings: dict[str, object],
    skill_disallowed: bool,
    report_findings_disallowed: bool,
) -> None:
  command = claude_code.ClaudeCodeBackend(run_kind=run_kind)._build_command("hi")

  assert command.count("--settings") == 1
  assert _settings_of(command) == expected_settings
  disallowed_tools = _disallowed_tool_values(command)
  assert ("Skill" in disallowed_tools) is skill_disallowed
  assert ("ReportFindings" in disallowed_tools) is report_findings_disallowed
  assert "ShareOnboardingGuide" in disallowed_tools


def test_manager_turn_settings_install_six_compaction_context_slots() -> None:
  session_id = "manager-session"
  command = claude_code.ClaudeCodeBackend(run_kind="manager_turn", cgroup_session_id=session_id)._build_command("hi")
  settings = _settings_of(command)
  [session_start] = settings.pop("hooks")["SessionStart"]

  assert command.count("--settings") == 1
  assert settings == {"disableClaudeAiConnectors": True, "syncClaudeAiSkills": False}
  assert session_start["matcher"] == "compact"
  assert [hook["type"] for hook in session_start["hooks"]] == ["command"] * 6
  assert [hook["command"] for hook in session_start["hooks"]
         ] == [f"charliebot compaction-context --session {session_id} --slot {slot}" for slot in range(1, 7)]


def test_manager_turn_settings_require_the_charliebot_session_id() -> None:
  with pytest.raises(ValueError, match="require a CharlieBot session id"):
    claude_code.ClaudeCodeBackend(run_kind="manager_turn")


def test_fast_mode_merges_into_the_single_settings_object() -> None:
  command = claude_code.ClaudeCodeBackend(run_kind="work", fast_mode=True)._build_command("hi")

  assert command.count("--settings") == 1
  assert _settings_of(command) == {
      "disableClaudeAiConnectors": True,
      "syncClaudeAiSkills": False,
      "disableBundledSkills": True,
      "fastMode": True,
  }
