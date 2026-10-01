import io
import shutil
import types
from pathlib import Path

import pytest

from src.agents.backends import pty_common
from src.cli import claude_sub
from src.cli.claude_sub_bridge import (
    HookProtocolError,
    HookTurnState,
)

SESSION_ID = "session-id"
WORKING_DIRECTORY = "/tmp/claude-sub-test"
PROMPT = "Fix the race\nwith details on later lines"


def _payload(event_name: str, **fields: object) -> dict:
  payload = {
      "hook_event_name": event_name,
      "session_id": SESSION_ID,
      "cwd": WORKING_DIRECTORY,
      **fields,
  }
  if event_name == "SessionStart":
    payload.setdefault("model", "claude-opus-4-8")
  if event_name == "Notification":
    payload.setdefault("message", "Claude is waiting for your input")
  return payload


def _state(prompt: str) -> HookTurnState:
  return HookTurnState(
      expected_session_id=SESSION_ID,
      expected_cwd=WORKING_DIRECTORY,
      expected_prompt=prompt,
      expected_source="startup",
      model="claude-opus-4-8",
  )


def _started_turn() -> HookTurnState:
  state = _state(PROMPT)
  state.handle("SessionStart", _payload("SessionStart", source="startup"))
  state.handle(
      "UserPromptSubmit",
      _payload("UserPromptSubmit", prompt=PROMPT, turn_id="turn-1"),
  )
  return state


def _stop_payload(**fields: object) -> dict:
  values = {
      "stop_hook_active": False,
      "last_assistant_message": "final answer",
      "background_tasks": [],
      "session_crons": [],
      "turn_id": "turn-1",
  }
  values.update(fields)
  return _payload("Stop", **values)


def test_parse_argv_rejects_non_stream_json_output() -> None:
  with pytest.raises(ValueError, match="stream-json"):
    claude_sub.parse_argv(["-p", "--output-format", "json"])


def test_main_reads_exactly_one_prompt_from_stdin(monkeypatch: pytest.MonkeyPatch) -> None:
  captured: dict[str, claude_sub.ClaudeSubArgs] = {}

  async def fake_run(args: claude_sub.ClaudeSubArgs) -> None:
    captured["args"] = args

  monkeypatch.setattr(claude_sub, "_run", fake_run)
  monkeypatch.setattr(claude_sub.sys, "stdin", io.StringIO("hello\nworld"))

  assert claude_sub.main(["-p", "--output-format", "stream-json"]) == 0
  assert captured["args"].prompt == "hello\nworld"


def test_missing_required_message_field_fails_loudly() -> None:
  state = _started_turn()
  payload = _payload(
      "MessageDisplay",
      turn_id="turn-1",
      message_id="message-1",
      index=0,
      final=True,
  )

  with pytest.raises(HookProtocolError, match="delta"):
    state.handle("MessageDisplay", payload)


def test_completion_requires_stop_then_idle_prompt() -> None:
  state = _started_turn()

  with pytest.raises(HookProtocolError, match="before Stop"):
    state.handle("Notification", _payload("Notification", notification_type="idle_prompt"))

  assert state.stop_seen is False
  state.handle("Stop", _stop_payload())
  assert state.stop_candidate == "final answer"
  assert state.idle_seen is False
  state.handle("Notification", _payload("Notification", notification_type="idle_prompt"))
  assert state.idle_seen is True


@pytest.mark.asyncio
async def test_old_style_live_pane_is_migration_blocked_without_killing_it(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  marker_states: list[claude_sub.SessionMarkerState] = []

  async def fake_exists(session_id: str) -> bool:
    return True

  async def fake_pane(session_id: str) -> claude_sub.PaneInfo:
    return claude_sub.PaneInfo(
        pid=1234,
        cwd=str(tmp_path),
        command="claude",
        dead=False,
    )

  monkeypatch.setattr(claude_sub, "_read_marker", lambda session_id: None)
  monkeypatch.setattr(pty_common, "tmux_session_exists", fake_exists)
  monkeypatch.setattr(claude_sub, "_pane_info", fake_pane)
  monkeypatch.setattr(claude_sub, "_write_marker", lambda session_id, state: marker_states.append(state))

  with pytest.raises(claude_sub.ClaudeSubError, match="old-style live Claude TUI"):
    await claude_sub._prepare_tmux_session(SESSION_ID, tmp_path, requested_resume=True)

  assert marker_states == [claude_sub.SessionMarkerState.MIGRATION_BLOCKED]


def _write_launch_plugin(tmp_path: Path, socket_name: str = "bridge.sock", token: str = "token-a") -> Path:
  bridge = types.SimpleNamespace(socket_path=tmp_path / socket_name, token=token)
  return claude_sub._write_hook_plugin(tmp_path, bridge)


def test_plugin_validate_key_masks_per_launch_values_and_pins_the_binary(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  fake_binary = tmp_path / "claude-bin"
  fake_binary.write_bytes(b"binary")
  monkeypatch.setattr(shutil, "which", lambda name: str(fake_binary))

  first = _write_launch_plugin(tmp_path / "one")
  second = _write_launch_plugin(tmp_path / "two", socket_name="other.sock", token="token-b")
  key_one = claude_sub._plugin_validate_key(first)
  key_two = claude_sub._plugin_validate_key(second)
  assert key_one is not None
  assert key_one == key_two

  plugin_json = first / ".claude-plugin" / "plugin.json"
  plugin_json.write_text(plugin_json.read_text(encoding="utf-8").replace("0.1.0", "0.2.0"), encoding="utf-8")
  assert claude_sub._plugin_validate_key(first) != key_one

  moved_binary = tmp_path / "claude-bin-2"
  moved_binary.write_bytes(b"binary-two")
  monkeypatch.setattr(shutil, "which", lambda name: str(moved_binary))
  assert claude_sub._plugin_validate_key(second) != key_two

  monkeypatch.setattr(shutil, "which", lambda name: None)
  assert claude_sub._plugin_validate_key(second) is None


def test_plugin_validate_key_degrades_to_validate_on_unreadable_inputs(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  fake_binary = tmp_path / "claude-bin"
  fake_binary.write_bytes(b"binary")
  monkeypatch.setattr(shutil, "which", lambda name: str(fake_binary))
  plugin_dir = _write_launch_plugin(tmp_path / "one")
  (plugin_dir / "hooks" / "hooks.json").write_text("{not json", encoding="utf-8")
  assert claude_sub._plugin_validate_key(plugin_dir) is None


@pytest.mark.asyncio
async def test_validate_hook_plugin_pair_cache_skips_the_second_pass(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  monkeypatch.setenv("CHARLIEBOT_HOME", str(tmp_path / "home"))
  fake_binary = tmp_path / "claude-bin"
  fake_binary.write_bytes(b"binary")
  monkeypatch.setattr(shutil, "which", lambda name: str(fake_binary))
  calls: list[list[str]] = []

  async def fake_capture(*args: str) -> tuple[int, str, str]:
    calls.append(list(args))
    return 0, "", ""

  monkeypatch.setattr(claude_sub, "_run_cli_capture", fake_capture)
  plugin_dir = _write_launch_plugin(tmp_path / "launch")

  await claude_sub._validate_hook_plugin(plugin_dir)
  await claude_sub._validate_hook_plugin(plugin_dir)
  assert len(calls) == 1

  (tmp_path / "home" / "claude-sub-sessions" / "plugin-validate-cache.json").unlink()
  await claude_sub._validate_hook_plugin(plugin_dir)
  assert len(calls) == 2

  hooks_json = plugin_dir / "hooks" / "hooks.json"
  hooks_json.write_text(hooks_json.read_text(encoding="utf-8").replace('"-S"', '"-S", "-I"'), encoding="utf-8")
  await claude_sub._validate_hook_plugin(plugin_dir)
  assert len(calls) == 3


@pytest.mark.asyncio
async def test_validate_hook_plugin_failure_writes_no_cache(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  monkeypatch.setenv("CHARLIEBOT_HOME", str(tmp_path / "home"))
  fake_binary = tmp_path / "claude-bin"
  fake_binary.write_bytes(b"binary")
  monkeypatch.setattr(shutil, "which", lambda name: str(fake_binary))
  calls: list[list[str]] = []

  async def failing_capture(*args: str) -> tuple[int, str, str]:
    calls.append(list(args))
    return 1, "", "schema rejected"

  monkeypatch.setattr(claude_sub, "_run_cli_capture", failing_capture)
  plugin_dir = _write_launch_plugin(tmp_path / "launch")

  with pytest.raises(claude_sub.ClaudeSubError, match="validation failed"):
    await claude_sub._validate_hook_plugin(plugin_dir)
  assert len(calls) == 1
  assert not (tmp_path / "home" / "claude-sub-sessions" / "plugin-validate-cache.json").exists()
