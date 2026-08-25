from pathlib import Path

import pytest

from src.agents.backends.antigravity_cli import AntigravityCliBackend
from src.agents.backends.registry import build_backend
from src.core.config import CharlieBotConfig
from src.core.models import BackendOption


def _build_backend(monkeypatch, **kwargs) -> AntigravityCliBackend:
  monkeypatch.setattr(
      "src.agents.backends.antigravity_cli.resolve_binary",
      lambda name, fallback: "/usr/bin/agy",
  )
  return AntigravityCliBackend(**kwargs)


def _write_fake_agy(tmp_path: Path, body: str) -> Path:
  fake_agy = tmp_path / "agy"
  fake_agy.write_text("#!/bin/sh\n" + body, encoding="utf-8")
  fake_agy.chmod(0o755)
  return fake_agy


async def _consume(backend: AntigravityCliBackend, cwd: Path) -> list[dict]:
  events: list[dict] = []
  async for event in backend.run("hello from CharlieBot", str(cwd), {"PATH": "/usr/bin:/bin"}):
    events.append(event)
  return events


async def _consume_raising(backend: AntigravityCliBackend, cwd: Path) -> list[dict]:
  """Consume events until the generator raises; re-raise, leaving collected events inspectable."""
  events: list[dict] = []
  gen = backend.run("hello from CharlieBot", str(cwd), {"PATH": "/usr/bin:/bin"})
  while True:
    try:
      event = await gen.__anext__()
    except StopAsyncIteration:
      return events
    except BaseException:
      raise
    events.append(event)


def test_build_command_passes_prompt_as_print_flag_value(monkeypatch) -> None:
  backend = _build_backend(monkeypatch, extra_flags=["--sandbox"])

  cmd = backend._build_command("--dash-prefixed prompt")

  assert cmd == [
      "/usr/bin/agy",
      "--print=--dash-prefixed prompt",
      "--print-timeout",
      "24h",
      "--dangerously-skip-permissions",
      "--output-format",
      "json",
      "--sandbox",
  ]
  assert "--model" not in cmd


def test_build_command_prepends_instructions_to_prompt(monkeypatch) -> None:
  backend = _build_backend(monkeypatch, instructions_content="# Antigravity Instructions\nBuild stuff.")

  cmd = backend._build_command("do the work")

  expected_prompt = "<system-instructions>\n# Antigravity Instructions\nBuild stuff.\n</system-instructions>\n\ndo the work"
  assert cmd[1] == f"--print={expected_prompt}"


def test_prepare_cwd_does_not_write_agents_md_when_instructions_provided(monkeypatch, tmp_path: Path) -> None:
  backend = _build_backend(monkeypatch, instructions_content="# Antigravity Instructions\nBuild stuff.")

  backend._prepare_cwd(str(tmp_path))

  agents_md = tmp_path / "AGENTS.md"
  assert not agents_md.exists()


def test_prepare_cwd_skips_agents_md_when_no_instructions(monkeypatch, tmp_path: Path) -> None:
  backend = _build_backend(monkeypatch)

  backend._prepare_cwd(str(tmp_path))

  assert not (tmp_path / "AGENTS.md").exists()


def test_build_command_append_conversation_flag_exactly_once_when_resuming(monkeypatch) -> None:
  monkeypatch.setattr(
      "src.agents.backends.antigravity_cli.resolve_binary",
      lambda name, fallback: "/usr/bin/agy",
  )

  backend = AntigravityCliBackend(resume_session_id="session-123")

  cmd = backend._build_command("hello")

  assert cmd.count("--conversation") == 1
  assert cmd[cmd.index("--conversation") + 1] == "session-123"


def test_build_command_never_uses_continue_flag(monkeypatch) -> None:
  monkeypatch.setattr(
      "src.agents.backends.antigravity_cli.resolve_binary",
      lambda name, fallback: "/usr/bin/agy",
  )

  fresh = AntigravityCliBackend()
  resumed = AntigravityCliBackend(resume_session_id="session-123")

  assert "--continue" not in fresh._build_command("hello")
  assert "--continue" not in resumed._build_command("hello")


@pytest.mark.asyncio
async def test_run_translates_envelope_into_session_text_and_result_events(monkeypatch, tmp_path: Path) -> None:
  fake_agy = _write_fake_agy(
      tmp_path,
      """
cat <<'JSON'
{"status":"SUCCESS","conversation_id":"conv-abc","num_turns":2,"response":"the answer","usage":{"input_tokens":10,"output_tokens":12,"thinking_tokens":11,"cache_read_tokens":3}}
JSON
""",
  )
  monkeypatch.setattr(
      "src.agents.backends.antigravity_cli.resolve_binary",
      lambda name, fallback: str(fake_agy),
  )
  log_dir = tmp_path / "logs"
  backend = AntigravityCliBackend(log_dir=log_dir)

  events = await _consume(backend, tmp_path)

  assert [e.get("type") for e in events] == [None, "assistant", "result"]
  assert events[0] == {"session_id": "conv-abc"}
  assert events[1] == {
      "type": "assistant",
      "message": {
          "content": [{
              "type": "text",
              "text": "the answer",
          }]
      },
  }
  assert events[2]["type"] == "result"
  assert events[2]["usage"]["input_tokens"] == 10
  assert events[2]["usage"]["output_tokens"] == 23
  assert events[2]["usage"]["cache_read_input_tokens"] == 3
  assert backend.exit_code == 0
  assert (log_dir / "stdout.log").read_text(encoding="utf-8") != ""
  assert (log_dir / "stderr.log").exists()


@pytest.mark.asyncio
async def test_run_emits_nonzero_stdout_as_error(monkeypatch, tmp_path: Path) -> None:
  fake_agy = _write_fake_agy(
      tmp_path,
      """
printf 'fatal from agy\\n'
exit 7
""",
  )
  monkeypatch.setattr(
      "src.agents.backends.antigravity_cli.resolve_binary",
      lambda name, fallback: str(fake_agy),
  )
  backend = AntigravityCliBackend()

  events = await _consume(backend, tmp_path)

  assert events == [{
      "type": "error",
      "message": "fatal from agy",
      "content": "fatal from agy",
  }]
  assert backend.exit_code == 7


def test_prepare_env_strips_api_keys_for_oauth(monkeypatch) -> None:
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


def test_registry_builds_antigravity_backend(monkeypatch) -> None:
  monkeypatch.setattr(
      "src.agents.backends.antigravity_cli.resolve_binary",
      lambda name, fallback: "/usr/bin/agy",
  )
  option = BackendOption(id="agy", label="Antigravity", type="antigravity")
  backend = build_backend(option, CharlieBotConfig(), extra_flags=["--sandbox"])

  assert isinstance(backend, AntigravityCliBackend)
  assert backend._model is None
  assert backend._build_command("hi") == [
      "/usr/bin/agy",
      "--print=hi",
      "--print-timeout",
      "24h",
      "--dangerously-skip-permissions",
      "--output-format",
      "json",
      "--sandbox",
  ]


@pytest.mark.asyncio
async def test_envelope_error_status_yields_error_event(monkeypatch, tmp_path: Path) -> None:
  fake_agy = _write_fake_agy(
      tmp_path,
      """
printf '%s' '{"status":"ERROR","error":"rate limited"}'
""",
  )
  monkeypatch.setattr(
      "src.agents.backends.antigravity_cli.resolve_binary",
      lambda name, fallback: str(fake_agy),
  )
  backend = AntigravityCliBackend()

  events = await _consume(backend, tmp_path)

  assert events == [{
      "type": "error",
      "message": "rate limited",
      "content": "rate limited",
  }]


@pytest.mark.asyncio
async def test_success_envelope_missing_conversation_id_triggers_guard(monkeypatch, tmp_path: Path) -> None:
  fake_agy = _write_fake_agy(
      tmp_path,
      """
printf '%s' '{"status":"SUCCESS","response":"hi"}'
""",
  )
  monkeypatch.setattr(
      "src.agents.backends.antigravity_cli.resolve_binary",
      lambda name, fallback: str(fake_agy),
  )
  backend = AntigravityCliBackend()

  with pytest.raises(ValueError, match="missing conversation_id"):
    events = await _consume_raising(backend, tmp_path)
    assert len(events) == 1 and events[0]["type"] == "error"
    assert "session_id" not in events[0]


@pytest.mark.asyncio
async def test_resume_envelope_id_mismatch_triggers_guard(monkeypatch, tmp_path: Path) -> None:
  fake_agy = _write_fake_agy(
      tmp_path,
      """
printf '%s' '{"status":"SUCCESS","conversation_id":"fresh-id","response":"hi","usage":{}}'
""",
  )
  monkeypatch.setattr(
      "src.agents.backends.antigravity_cli.resolve_binary",
      lambda name, fallback: str(fake_agy),
  )
  backend = AntigravityCliBackend(resume_session_id="anchor-id")

  with pytest.raises(ValueError, match="does not match anchor anchor-id"):
    events = await _consume_raising(backend, tmp_path)
    assert len(events) == 1 and events[0]["type"] == "error"
    assert "session_id" not in events[0]


@pytest.mark.asyncio
async def test_non_envelope_stdout_exit_zero_triggers_guard(monkeypatch, tmp_path: Path) -> None:
  fake_agy = _write_fake_agy(
      tmp_path,
      """
printf 'plain text, not json'
""",
  )
  monkeypatch.setattr(
      "src.agents.backends.antigravity_cli.resolve_binary",
      lambda name, fallback: str(fake_agy),
  )
  backend = AntigravityCliBackend()

  with pytest.raises(ValueError, match="non-json stdout"):
    events = await _consume_raising(backend, tmp_path)
    assert len(events) == 1 and events[0]["type"] == "error"
    assert "session_id" not in events[0]


@pytest.mark.asyncio
async def test_bare_session_id_event_is_adopted_as_anchor_by_handle_event(
    monkeypatch, tmp_path: Path
) -> None:
  fake_agy = _write_fake_agy(
      tmp_path,
      """
printf '%s' '{"status":"SUCCESS","conversation_id":"conv-abc","response":"hi","usage":{}}'
""",
  )
  monkeypatch.setattr(
      "src.agents.backends.antigravity_cli.resolve_binary",
      lambda name, fallback: str(fake_agy),
  )
  backend = AntigravityCliBackend()

  from src.agents.master_cc_run import _handle_event

  events = await _consume(backend, tmp_path)
  assert events[0] == {"session_id": "conv-abc"}

  captured: list[dict] = []

  async def fake_persist(session_id: str, ev: dict) -> None:
    captured.append(ev)

  cc_session_id = await _handle_event(events[0], "session-id", None, fake_persist)

  assert cc_session_id == "conv-abc"
