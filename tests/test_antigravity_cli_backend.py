from __future__ import annotations

from pathlib import Path
from typing import Any

import pytest
from conftest import (
    AGY_BACKEND_OPTION,
    ANTIGRAVITY_RESOLVE_BINARY_PATCH_TARGET,
    assistant_text_event,
    build_cli_backend_rig,
)

from src.agents.backends.antigravity_cli import AntigravityCliBackend
from src.agents.backends.registry import build_backend
from src.core import event_types as ET
from src.core.config import CharlieBotConfig


def _build_backend(monkeypatch: pytest.MonkeyPatch, **kwargs: Any) -> AntigravityCliBackend:
  return build_cli_backend_rig(monkeypatch, AntigravityCliBackend, **kwargs)


def _write_fake_agy(tmp_path: Path, body: str) -> Path:
  fake_agy = tmp_path / "agy"
  fake_agy.write_text("#!/bin/sh\n" + body, encoding="utf-8")
  fake_agy.chmod(0o755)
  return fake_agy


def _install_fake_agy(monkeypatch: pytest.MonkeyPatch, tmp_path: Path, body: str) -> None:
  fake_agy = _write_fake_agy(tmp_path, body)
  monkeypatch.setattr(
      ANTIGRAVITY_RESOLVE_BINARY_PATCH_TARGET,
      lambda name, fallback: str(fake_agy),
  )


async def _consume(backend: AntigravityCliBackend, cwd: Path) -> list[dict]:
  return [event async for event in backend.run("hello from CharlieBot", str(cwd), {"PATH": "/usr/bin:/bin"})]


def test_build_command_passes_prompt_as_print_flag_value(monkeypatch: pytest.MonkeyPatch) -> None:
  backend = _build_backend(monkeypatch, extra_flags=["--sandbox"])

  cmd = backend._build_command("--dash-prefixed prompt")

  assert cmd == [
      "/usr/bin/agy",
      "--print=--dash-prefixed prompt",
      "--print-timeout",
      "1h",
      "--dangerously-skip-permissions",
      "--output-format",
      "json",
      "--sandbox",
  ]
  assert "--model" not in cmd


def test_build_command_prepends_instructions_to_prompt(monkeypatch: pytest.MonkeyPatch) -> None:
  backend = _build_backend(monkeypatch, instructions_content="# Antigravity Instructions\nBuild stuff.")

  cmd = backend._build_command("do the work")

  expected_prompt = (
      "<system-instructions>\n# Antigravity Instructions\nBuild stuff.\n"
      "</system-instructions>\n\ndo the work")
  assert cmd[1] == f"--print={expected_prompt}"


@pytest.mark.asyncio
async def test_run_translates_envelope_into_session_text_and_result_events(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  _install_fake_agy(
      monkeypatch,
      tmp_path,
      """
cat <<'JSON'
{"status":"SUCCESS","conversation_id":"conv-abc","num_turns":2,
"response":"the answer","usage":{"input_tokens":10,"output_tokens":12,"thinking_tokens":11,"cache_read_tokens":3}}
JSON
""",
  )
  log_dir = tmp_path / "logs"
  backend = AntigravityCliBackend(log_dir=log_dir)

  events = await _consume(backend, tmp_path)

  assert [e.get("type") for e in events] == [ET.SESSION_ATTACHED, "assistant", "result"]
  from src.agents.master_cc_run import _handle_event

  adopted_events: list[dict] = []

  async def fake_persist(session_id: str, event: dict) -> None:
    adopted_events.append(event)

  assert await _handle_event(events[0], "session-id", None, fake_persist) == "conv-abc"
  assert adopted_events == [events[0]]
  assert events[1] == assistant_text_event("the answer")
  assert events[2]["type"] == "result"
  assert events[2]["usage"]["input_tokens"] == 10
  assert events[2]["usage"]["output_tokens"] == 23
  assert events[2]["usage"]["cache_read_input_tokens"] == 3
  assert backend.exit_code == 0
  assert (log_dir / "stdout.log").read_text(encoding="utf-8") != ""
  assert (log_dir / "stderr.log").exists()


def test_prepare_env_strips_api_keys_for_oauth(monkeypatch: pytest.MonkeyPatch) -> None:
  backend = _build_backend(monkeypatch)

  env = backend._prepare_env(
      {
          "PATH": "/usr/bin",
          "GEMINI_API_KEY": "secret-gemini-key",
          "GOOGLE_API_KEY": "secret-google-key",
          "OTHER_VAR": "keep-me",
      })

  assert "GEMINI_API_KEY" not in env
  assert "GOOGLE_API_KEY" not in env
  assert env.get("OTHER_VAR") == "keep-me"
  assert "/usr/bin" in env.get("PATH", "")


def test_registry_builds_antigravity_backend(monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setattr(
      ANTIGRAVITY_RESOLVE_BINARY_PATCH_TARGET,
      lambda name, fallback: "/usr/bin/agy",
  )
  option = AGY_BACKEND_OPTION
  backend = build_backend(option, CharlieBotConfig(), extra_flags=["--sandbox"])

  assert isinstance(backend, AntigravityCliBackend)
  assert backend._model is None
  assert backend._build_command("hi") == [
      "/usr/bin/agy",
      "--print=hi",
      "--print-timeout",
      "1h",
      "--dangerously-skip-permissions",
      "--output-format",
      "json",
      "--sandbox",
  ]


@pytest.mark.asyncio
async def test_typed_session_attach_event_is_adopted_as_anchor_by_handle_event(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  _install_fake_agy(
      monkeypatch,
      tmp_path,
      """
printf '%s' '{"status":"SUCCESS","conversation_id":"conv-abc","response":"hi","usage":{}}'
""",
  )
  backend = AntigravityCliBackend()

  from src.agents.master_cc_run import _handle_event

  events = await _consume(backend, tmp_path)

  captured: list[dict] = []

  async def fake_persist(session_id: str, ev: dict) -> None:
    captured.append(ev)

  cc_session_id = await _handle_event(events[0], "session-id", None, fake_persist)

  assert cc_session_id == "conv-abc"
  assert captured == [events[0]]
