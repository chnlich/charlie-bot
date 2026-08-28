import asyncio
import io
import json
import os
import stat
from pathlib import Path

import pytest

from src.cli import claude_sub, claude_sub_hook
from src.cli.claude_sub_bridge import (
  HookBridge,
  HookProtocolError,
  HookTurnState,
  PromptDelivery,
)
from src.core import event_types as ET

SESSION_ID = "session-id"
WORKING_DIRECTORY = "/tmp/claude-sub-test"
PROMPT = "Fix the race\nwith details on later lines"


def _payload(event_name: str, **fields) -> dict:
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


def _state(prompt: str = PROMPT) -> HookTurnState:
  return HookTurnState(
      expected_session_id=SESSION_ID,
      expected_cwd=WORKING_DIRECTORY,
      expected_prompt=prompt,
      expected_source="startup",
      model="claude-opus-4-8",
  )


def _started_turn(prompt: str = PROMPT) -> HookTurnState:
  state = _state(prompt)
  state.handle("SessionStart", _payload("SessionStart", source="startup"))
  state.handle(
      "UserPromptSubmit",
      _payload("UserPromptSubmit", prompt=prompt, turn_id="turn-1"),
  )
  return state


def _stop_payload(**fields) -> dict:
  values = {
      "stop_hook_active": False,
      "last_assistant_message": "final answer",
      "background_tasks": [],
      "session_crons": [],
      "turn_id": "turn-1",
  }
  values.update(fields)
  return _payload("Stop", **values)


def test_parse_argv_preserves_existing_backend_options() -> None:
  args = claude_sub.parse_argv(
      [
          "-p",
          "--output-format",
          "stream-json",
          "--verbose",
          "--dangerously-skip-permissions",
          "--disallowed-tools",
          "Monitor,CronCreate",
          "--model",
          "claude-opus-4-8",
          "--effort",
          "max",
          "--session-id",
          "fresh-session-id",
          "--resume",
          "session-id",
          "--settings",
          '{"fastMode":true}',
      ])

  assert args.output_format == "stream-json"
  assert args.prompt == ""
  assert args.model == "claude-opus-4-8"
  assert args.effort == "max"
  assert args.session_id == "fresh-session-id"
  assert args.resume == "session-id"
  assert args.disallowed_tools == ["Monitor,CronCreate"]
  assert args.settings == ['{"fastMode":true}']


def test_parse_argv_rejects_non_stream_json_output() -> None:
  with pytest.raises(ValueError, match="stream-json"):
    claude_sub.parse_argv(["-p", "--output-format", "json"])


def test_parse_argv_rejects_positional_prompt_tokens() -> None:
  with pytest.raises(ValueError, match="stdin"):
    claude_sub.parse_argv(["-p", "--output-format", "stream-json", "--", "hello"])


def test_main_reads_exactly_one_prompt_from_stdin(monkeypatch: pytest.MonkeyPatch) -> None:
  captured: dict[str, claude_sub.ClaudeSubArgs] = {}

  async def fake_run(args: claude_sub.ClaudeSubArgs) -> None:
    captured["args"] = args

  monkeypatch.setattr(claude_sub, "_run", fake_run)
  monkeypatch.setattr(claude_sub.sys, "stdin", io.StringIO("hello\nworld"))

  assert claude_sub.main(["-p", "--output-format", "stream-json"]) == 0
  assert captured["args"].prompt == "hello\nworld"


def test_validate_prompt_reports_nul_and_size_causes() -> None:
  with pytest.raises(ValueError, match="NUL"):
    claude_sub.validate_prompt("before\x00after")

  with pytest.raises(ValueError, match="exceeding"):
    claude_sub.validate_prompt("x" * (claude_sub.MAX_PROMPT_BYTES + 1))


def test_leading_dash_prompt_is_one_protected_argv_element(tmp_path: Path) -> None:
  args = claude_sub.ClaudeSubArgs(
      output_format="stream-json",
      prompt="-do not parse this as an option",
  )

  claude_sub.validate_prompt(args.prompt)
  argv = claude_sub._build_claude_argv(args, SESSION_ID, False, tmp_path / "plugin")

  assert argv[-2:] == ["--", args.prompt]
  assert argv.count(args.prompt) == 1


def test_prompt_delivery_starts_unknown_and_acknowledges_exact_submit() -> None:
  state = _state()

  assert state.delivery is PromptDelivery.UNKNOWN
  state.handle("SessionStart", _payload("SessionStart", source="startup"))
  state.handle(
      "UserPromptSubmit",
      _payload("UserPromptSubmit", prompt=PROMPT, turn_id="turn-1", future_field="ignored"),
  )

  assert state.delivery is PromptDelivery.ACKNOWLEDGED
  assert state.correlation_field == "turn_id"
  assert state.correlation_id == "turn-1"


@pytest.mark.parametrize(
    "fields",
    [
        {"prompt": "different prompt", "turn_id": "turn-1"},
        {"prompt": PROMPT, "turn_id": "turn-1", "session_id": "other-session"},
        {"prompt": PROMPT, "turn_id": "turn-1", "cwd": "/tmp/other-directory"},
    ],
)
def test_prompt_delivery_mismatch_blocks_and_never_acknowledges(fields: dict) -> None:
  state = _state()
  state.handle("SessionStart", _payload("SessionStart", source="startup"))

  with pytest.raises(HookProtocolError, match="mismatch"):
    state.handle("UserPromptSubmit", _payload("UserPromptSubmit", **fields))

  assert state.delivery is PromptDelivery.UNKNOWN


def test_user_prompt_submit_requires_a_correlation_field() -> None:
  state = _state()
  state.handle("SessionStart", _payload("SessionStart", source="startup"))

  with pytest.raises(HookProtocolError, match="correlation"):
    state.handle("UserPromptSubmit", _payload("UserPromptSubmit", prompt=PROMPT))


def test_hook_mapping_covers_message_tools_compaction_and_unknown_fields() -> None:
  state = _started_turn()

  message_events = state.handle(
      "MessageDisplay",
      _payload(
          "MessageDisplay",
          turn_id="turn-1",
          message_id="message-1",
          index=0,
          final=False,
          delta="assistant text",
          transcript_path="/must-not-be-read",
          unknown_non_control={"ignored": True},
      ))
  assert message_events[0]["type"] == ET.ASSISTANT
  assert message_events[0]["message"]["content"] == [{"type": "text", "text": "assistant text"}]
  assert message_events[0]["message_id"] == "message-1"

  tool_events = state.handle(
      "PreToolUse",
      _payload(
          "PreToolUse",
          tool_name="Bash",
          tool_input={"command": "printf test"},
          tool_use_id="tool-1",
          unknown_non_control="ignored",
      ))
  assert tool_events[0]["message"]["content"][0]["id"] == "tool-1"

  result_events = state.handle(
      "PostToolUse",
      _payload(
          "PostToolUse",
          tool_name="Bash",
          tool_input={"command": "printf test"},
          tool_use_id="tool-1",
          tool_response={"content": "test"},
      ))
  assert result_events[0]["message"]["content"][0]["tool_use_id"] == "tool-1"
  assert result_events[0]["message"]["content"][0]["is_error"] is False

  failure_events = state.handle(
      "PostToolUseFailure",
      _payload(
          "PostToolUseFailure",
          tool_name="Bash",
          tool_input={"command": "false"},
          tool_use_id="tool-2",
          error="command failed",
          is_interrupt=False,
      ))
  assert failure_events[0]["message"]["content"][0]["tool_use_id"] == "tool-2"
  assert failure_events[0]["message"]["content"][0]["is_error"] is True

  compact_events = state.handle(
      "PostCompact",
      _payload(
          "PostCompact",
          trigger="auto",
          pre_tokens=123,
          compact_summary="summary",
          unknown_non_control=True,
      ))
  assert compact_events == [{
      "type": ET.CONTEXT_COMPACTED,
      "trigger": "auto",
      "pre_tokens": 123,
  }]


def test_message_display_uses_turn_id_for_the_wrapped_event() -> None:
  state = _state()
  state.handle("SessionStart", _payload("SessionStart", source="startup"))
  state.handle(
      "UserPromptSubmit",
      _payload("UserPromptSubmit", prompt=PROMPT, prompt_id="prompt-1"),
  )

  events = state.handle(
      "MessageDisplay",
      _payload(
          "MessageDisplay",
          prompt_id="prompt-1",
          turn_id="turn-1",
          message_id="message-1",
          index=0,
          final=True,
          delta="answer",
      ))

  assert events[0]["turn_id"] == "turn-1"


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


def test_unknown_control_enum_fails_loudly() -> None:
  state = _started_turn()

  with pytest.raises(HookProtocolError, match="unknown PostCompact trigger"):
    state.handle("PostCompact", _payload("PostCompact", trigger="future"))


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


def test_session_end_during_active_turn_fails() -> None:
  state = _started_turn()

  with pytest.raises(HookProtocolError, match="active turn"):
    state.handle("SessionEnd", _payload("SessionEnd", reason="logout"))


def test_stop_rejects_background_work_and_stop_failure_preserves_error() -> None:
  state = _started_turn()
  with pytest.raises(HookProtocolError, match="background work"):
    state.handle("Stop", _stop_payload(background_tasks=["task-1"]))

  state = _started_turn()
  events = state.handle(
      "StopFailure",
      _payload(
          "StopFailure",
          turn_id="turn-1",
          error="rate_limit",
          error_details="limit reached",
          last_assistant_message="partial answer",
      ))

  assert [event["type"] for event in events] == [ET.RATE_LIMIT_EVENT, ET.ERROR]
  assert events[0]["rate_limit_info"]["status"] == "rejected"
  assert events[1]["error"] == "rate_limit"
  assert events[1]["error_details"] == "limit reached"
  assert events[1]["message"] == "partial answer"
  assert state.failure is not None


@pytest.mark.asyncio
async def test_hook_bridge_accepts_fake_hook_source_and_emits_events(tmp_path: Path) -> None:
  bridge = HookBridge(tmp_path / "bridge.sock", "token", _state())
  await bridge.start()
  try:
    session_start = await asyncio.to_thread(
        claude_sub_hook._send_request,
        str(bridge.socket_path),
        bridge.token,
        False,
        _payload("SessionStart", source="startup"),
    )
    assert session_start == {"ok": True}
    assert (await bridge.events.get())["subtype"] == "init"

    user_submit = await asyncio.to_thread(
        claude_sub_hook._send_request,
        str(bridge.socket_path),
        bridge.token,
        True,
        _payload("UserPromptSubmit", prompt=PROMPT, turn_id="turn-1"),
    )
    assert user_submit == {"ok": True}
    message = await asyncio.to_thread(
        claude_sub_hook._send_request,
        str(bridge.socket_path),
        bridge.token,
        False,
        _payload(
            "MessageDisplay",
            turn_id="turn-1",
            message_id="message-1",
            index=0,
            final=True,
            delta="hello",
        ),
    )
    assert message == {"ok": True}
    assert (await bridge.events.get())["message_id"] == "message-1"
  finally:
    await bridge.stop()


def test_session_settings_are_session_scoped_and_lower_idle_threshold() -> None:
  args = claude_sub.ClaudeSubArgs(
      output_format="stream-json",
      settings=['{"fastMode":true,"messageIdleNotifThresholdMs":60000}'],
  )

  settings = json.loads(claude_sub._session_settings(args))
  assert settings["fastMode"] is True
  assert settings["skipDangerousModePermissionPrompt"] is True
  assert settings["messageIdleNotifThresholdMs"] == claude_sub._IDLE_NOTIFICATION_THRESHOLD_MS
  assert settings["inputNeededNotifEnabled"] is True

  with pytest.raises(claude_sub.ClaudeSubError, match="inline JSON"):
    claude_sub._session_settings(
        claude_sub.ClaudeSubArgs(output_format="stream-json", settings=["user-settings.json"]))


def test_session_config_overlay_sets_idle_threshold_without_touching_sources(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  source_global = tmp_path / "source-claude.json"
  source_settings = tmp_path / "source-settings.json"
  source_credentials = tmp_path / "source-credentials.json"
  source_remote = tmp_path / "source-remote-settings.json"
  source_global.write_text(json.dumps({"projects": {"/project": {}}}), encoding="utf-8")
  source_settings.write_text(json.dumps({"hooks": {"Stop": []}}), encoding="utf-8")
  source_credentials.write_text("credentials", encoding="utf-8")
  source_remote.write_text("{}", encoding="utf-8")
  monkeypatch.setattr(
      claude_sub,
      "_claude_user_config_paths",
      lambda: (source_global, source_settings, source_credentials, source_remote),
  )
  monkeypatch.setattr(claude_sub, "_session_marker_dir", lambda: tmp_path / "markers")

  config_dir = claude_sub._prepare_session_config(SESSION_ID, Path(WORKING_DIRECTORY))

  assert config_dir == tmp_path / "markers" / "configs" / SESSION_ID
  overlay_global = json.loads((config_dir / ".claude.json").read_text())
  assert overlay_global["messageIdleNotifThresholdMs"] == 1000
  assert overlay_global["projects"][WORKING_DIRECTORY] == {
      "hasTrustDialogAccepted": True,
      "projectOnboardingSeenCount": 1,
  }
  assert overlay_global["projects"]["/project"] == {}
  assert (config_dir / "settings.json").read_text() == source_settings.read_text()
  assert (config_dir / ".credentials.json").read_text() == "credentials"
  assert json.loads(source_global.read_text()) == {"projects": {"/project": {}}}


def _install_config_paths(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
    *,
    credentials_content: str | None = "credentials",
    global_content: dict | None = None,
) -> tuple[Path, Path, Path, Path]:
  source_global = tmp_path / "source-claude.json"
  source_settings = tmp_path / "source-settings.json"
  source_credentials = tmp_path / "source-credentials.json"
  source_remote = tmp_path / "source-remote-settings.json"
  source_global.write_text(json.dumps(global_content if global_content is not None else {}), encoding="utf-8")
  source_settings.write_text(json.dumps({"hooks": {"Stop": []}}), encoding="utf-8")
  if credentials_content is not None:
    source_credentials.write_text(credentials_content, encoding="utf-8")
  source_remote.write_text("{}", encoding="utf-8")
  monkeypatch.setattr(
      claude_sub,
      "_claude_user_config_paths",
      lambda: (source_global, source_settings, source_credentials, source_remote),
  )
  monkeypatch.setattr(claude_sub, "_session_marker_dir", lambda: tmp_path / "markers")
  return source_global, source_settings, source_credentials, source_remote


def test_session_config_overlay_heals_existing_overlay_missing_trust_entry(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  source_global, _, _, _ = _install_config_paths(monkeypatch, tmp_path)
  config_dir = claude_sub._session_config_dir(SESSION_ID)
  config_dir.mkdir(parents=True)
  overlay_global = config_dir / ".claude.json"
  overlay_global.write_text(
      json.dumps({
          "preservedKey": "kept",
          "messageIdleNotifThresholdMs": claude_sub._IDLE_NOTIFICATION_THRESHOLD_MS,
          "projects": {"/unrelated": {"hasTrustDialogAccepted": True}},
      }),
      encoding="utf-8",
  )

  claude_sub._prepare_session_config(SESSION_ID, Path(WORKING_DIRECTORY))

  data = json.loads(overlay_global.read_text())
  assert data["preservedKey"] == "kept"
  assert data["projects"]["/unrelated"] == {"hasTrustDialogAccepted": True}
  assert data["projects"][WORKING_DIRECTORY] == {
      "hasTrustDialogAccepted": True,
      "projectOnboardingSeenCount": 1,
  }
  assert json.loads(source_global.read_text()) == {}


def test_session_config_overlay_preserves_existing_project_onboarding_seen_count(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  _install_config_paths(monkeypatch, tmp_path)
  config_dir = claude_sub._session_config_dir(SESSION_ID)
  config_dir.mkdir(parents=True)
  overlay_global = config_dir / ".claude.json"
  overlay_global.write_text(
      json.dumps({
          "projects": {
              WORKING_DIRECTORY: {
                  "hasTrustDialogAccepted": False,
                  "projectOnboardingSeenCount": 7,
              },
          },
      }),
      encoding="utf-8",
  )

  claude_sub._prepare_session_config(SESSION_ID, Path(WORKING_DIRECTORY))

  project = json.loads(overlay_global.read_text())["projects"][WORKING_DIRECTORY]
  assert project["hasTrustDialogAccepted"] is True
  assert project["projectOnboardingSeenCount"] == 7


def test_session_config_overlay_idempotent_when_already_configured(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  _install_config_paths(monkeypatch, tmp_path)
  config_dir = claude_sub._session_config_dir(SESSION_ID)
  config_dir.mkdir(parents=True)
  overlay_global = config_dir / ".claude.json"
  overlay_global.write_text(
      json.dumps({
          "projects": {
              WORKING_DIRECTORY: {
                  "hasTrustDialogAccepted": True,
                  "projectOnboardingSeenCount": 3,
              },
          },
          "messageIdleNotifThresholdMs": claude_sub._IDLE_NOTIFICATION_THRESHOLD_MS,
      }),
      encoding="utf-8",
  )
  writes: list[Path] = []
  real_write = claude_sub._write_json_atomically

  def counting_write(path: Path, value: dict) -> None:
    writes.append(path)
    real_write(path, value)

  monkeypatch.setattr(claude_sub, "_write_json_atomically", counting_write)

  claude_sub._prepare_session_config(SESSION_ID, Path(WORKING_DIRECTORY))

  assert not writes


def test_session_credentials_recopied_when_source_strictly_newer(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  _, _, source_credentials, _ = _install_config_paths(monkeypatch, tmp_path)
  config_dir = claude_sub._prepare_session_config(SESSION_ID, Path(WORKING_DIRECTORY))
  credentials_target = config_dir / ".credentials.json"
  assert credentials_target.read_text() == "credentials"
  os.utime(credentials_target, (1000, 1000))
  source_credentials.write_text("rotated-credentials", encoding="utf-8")
  os.utime(source_credentials, (2000, 2000))

  claude_sub._prepare_session_config(SESSION_ID, Path(WORKING_DIRECTORY))

  assert credentials_target.read_text() == "rotated-credentials"
  assert stat.S_IMODE(os.stat(credentials_target).st_mode) == 0o600


def test_session_credentials_not_recopied_when_copy_same_age_or_newer(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  _, _, source_credentials, _ = _install_config_paths(monkeypatch, tmp_path)
  config_dir = claude_sub._prepare_session_config(SESSION_ID, Path(WORKING_DIRECTORY))
  credentials_target = config_dir / ".credentials.json"
  source_credentials.write_text("should-not-be-copied", encoding="utf-8")
  os.utime(source_credentials, (1000, 1000))
  os.utime(credentials_target, (2000, 2000))

  claude_sub._prepare_session_config(SESSION_ID, Path(WORKING_DIRECTORY))

  assert credentials_target.read_text() == "credentials"
  source_credentials.write_text("same-age-still-skipped", encoding="utf-8")
  os.utime(source_credentials, (1500, 1500))
  os.utime(credentials_target, (1500, 1500))
  claude_sub._prepare_session_config(SESSION_ID, Path(WORKING_DIRECTORY))
  assert credentials_target.read_text() == "credentials"


def test_session_credentials_source_missing_with_copy_present_proceeds(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  _, _, source_credentials, _ = _install_config_paths(monkeypatch, tmp_path)
  config_dir = claude_sub._prepare_session_config(SESSION_ID, Path(WORKING_DIRECTORY))
  credentials_target = config_dir / ".credentials.json"
  source_credentials.unlink()

  claude_sub._prepare_session_config(SESSION_ID, Path(WORKING_DIRECTORY))

  assert credentials_target.read_text() == "credentials"


def test_session_credentials_both_missing_raises(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  _install_config_paths(monkeypatch, tmp_path, credentials_content=None)
  config_dir = claude_sub._session_config_dir(SESSION_ID)
  config_dir.mkdir(parents=True)
  # No source credentials, no overlay copy.

  with pytest.raises(claude_sub.ClaudeSubError, match="credentials"):
    claude_sub._prepare_session_config(SESSION_ID, Path(WORKING_DIRECTORY))


def test_hook_plugin_registers_every_required_event_without_user_settings(tmp_path: Path) -> None:
  bridge = HookBridge(tmp_path / "bridge.sock", "per-turn-token", _state())
  plugin_dir = claude_sub._write_hook_plugin(tmp_path, bridge)
  plugin = json.loads((plugin_dir / ".claude-plugin" / "plugin.json").read_text())
  hooks = json.loads((plugin_dir / "hooks" / "hooks.json").read_text())

  assert plugin["name"] == "charliebot-hook-bridge"
  assert set(hooks["hooks"]) == set(claude_sub._HOOK_EVENTS)
  for event_name, groups in hooks["hooks"].items():
    if event_name == "Notification":
      assert "matcher" not in groups[0]
    command = groups[0]["hooks"][0]
    assert command["type"] == "command"
    assert command["command"] == claude_sub.sys.executable
    assert "--socket" in command["args"]
    assert ("--gate" in command["args"]) is (event_name in {
        "UserPromptSubmit",
        "PreToolUse",
        "PermissionRequest",
    })


@pytest.mark.asyncio
async def test_respawn_passes_one_prompt_directly_and_does_not_use_a_shell(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  calls: list[tuple[str, ...]] = []

  async def fake_tmux_checked(*args: str, **kwargs) -> str:
    calls.append(args)
    return ""

  monkeypatch.setattr(claude_sub, "_tmux_checked", fake_tmux_checked)
  args = claude_sub.ClaudeSubArgs(
      output_format="stream-json",
      prompt="-leading prompt",
      model="claude-opus-4-8",
      effort="max",
      disallowed_tools=["AskUserQuestion,ExitPlanMode"],
  )

  await claude_sub._respawn_claude(args, SESSION_ID, True, tmp_path / "plugin", Path.cwd())

  assert len(calls) == 1
  command = calls[0]
  assert command[0:4] == ("respawn-pane", "-k", "-t", claude_sub.tmux_session_name(SESSION_ID))
  assert "sh" not in command
  assert command[-2:] == ("--", "-leading prompt")
  assert command.count("-leading prompt") == 1


@pytest.mark.asyncio
async def test_respawn_passes_auto_compact_window_default_to_the_pane(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  calls: list[tuple[str, ...]] = []

  async def fake_tmux_checked(*args: str, **kwargs) -> str:
    calls.append(args)
    return ""

  monkeypatch.setattr(claude_sub, "_tmux_checked", fake_tmux_checked)
  monkeypatch.delenv("CLAUDE_CODE_AUTO_COMPACT_WINDOW", raising=False)
  args = claude_sub.ClaudeSubArgs(
      output_format="stream-json",
      prompt="hello",
      model="claude-opus-4-8",
      effort="max",
      disallowed_tools=["AskUserQuestion,ExitPlanMode"],
  )

  await claude_sub._respawn_claude(args, SESSION_ID, True, tmp_path / "plugin", Path.cwd())

  command = calls[0]
  assert "CLAUDE_CODE_AUTO_COMPACT_WINDOW=433000" in command
  assert command[command.index("CLAUDE_CODE_AUTO_COMPACT_WINDOW=433000") - 1] == "-e"


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
  monkeypatch.setattr(claude_sub, "tmux_session_exists", fake_exists)
  monkeypatch.setattr(claude_sub, "_pane_info", fake_pane)
  monkeypatch.setattr(claude_sub, "_write_marker", lambda session_id, state: marker_states.append(state))

  with pytest.raises(claude_sub.ClaudeSubError, match="old-style live Claude TUI"):
    await claude_sub._prepare_tmux_session(SESSION_ID, tmp_path, requested_resume=True)

  assert marker_states == [claude_sub.SessionMarkerState.MIGRATION_BLOCKED]


@pytest.mark.asyncio
async def test_migration_resumes_same_session_after_old_tui_exits_and_session_closes(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  marker_states: list[claude_sub.SessionMarkerState] = []
  created: list[str] = []

  async def fake_exists(session_id: str) -> bool:
    return False

  async def fake_create(session_id: str, cwd: Path) -> None:
    created.append(session_id)

  monkeypatch.setattr(
      claude_sub,
      "_read_marker",
      lambda session_id: claude_sub.SessionMarkerState.MIGRATION_BLOCKED,
  )
  monkeypatch.setattr(claude_sub, "tmux_session_exists", fake_exists)
  monkeypatch.setattr(claude_sub, "_create_tmux_host", fake_create)
  monkeypatch.setattr(claude_sub, "_write_marker", lambda session_id, state: marker_states.append(state))

  # Old sessions lack remain-on-exit, so exiting the old TUI closes the session.
  # The new path must still resume the same Claude session id rather than refuse.
  resume = await claude_sub._prepare_tmux_session(SESSION_ID, tmp_path, requested_resume=False)

  assert resume is True
  assert created == [SESSION_ID]
  assert not marker_states


@pytest.mark.asyncio
async def test_started_marker_without_session_refuses_to_resume(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  async def fake_exists(session_id: str) -> bool:
    return False

  monkeypatch.setattr(
      claude_sub,
      "_read_marker",
      lambda session_id: claude_sub.SessionMarkerState.STARTED_BY_NEW_ADAPTER,
  )
  monkeypatch.setattr(claude_sub, "tmux_session_exists", fake_exists)

  with pytest.raises(claude_sub.ClaudeSubError, match="refusing to resume"):
    await claude_sub._prepare_tmux_session(SESSION_ID, tmp_path, requested_resume=True)


def test_success_result_uses_stop_candidate_without_usage_or_cost() -> None:
  event = claude_sub._result_event(SESSION_ID, "final answer", 1234)

  assert event["result"] == "final answer"
  assert event["duration_ms"] == 1234
  assert event["usage"] == {}
  assert event["total_cost_usd"] is None
  assert "thinking" not in event
