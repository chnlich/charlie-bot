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
  kwargs.setdefault("model", "gemini-test-model")
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


def test_build_command_passes_prompt_as_print_flag_value(monkeypatch) -> None:
  backend = _build_backend(monkeypatch, model="gemini-test-model", extra_flags=["--sandbox"])

  cmd = backend._build_command("--dash-prefixed prompt")

  assert cmd == [
      "/usr/bin/agy",
      "--print",
      "--dash-prefixed prompt",
      "--print-timeout",
      "24h",
      "--dangerously-skip-permissions",
      "--sandbox",
  ]


def test_build_command_prepends_instructions_to_prompt(monkeypatch) -> None:
  backend = _build_backend(monkeypatch, instructions_content="# Antigravity Instructions\nBuild stuff.")

  cmd = backend._build_command("do the work")

  expected_prompt = "<system-instructions>\n# Antigravity Instructions\nBuild stuff.\n</system-instructions>\n\ndo the work"
  assert cmd[2] == expected_prompt


def test_prepare_cwd_does_not_write_agents_md_when_instructions_provided(monkeypatch, tmp_path: Path) -> None:
  backend = _build_backend(monkeypatch, instructions_content="# Antigravity Instructions\nBuild stuff.")

  backend._prepare_cwd(str(tmp_path))

  agents_md = tmp_path / "AGENTS.md"
  assert not agents_md.exists()


def test_prepare_cwd_skips_agents_md_when_no_instructions(monkeypatch, tmp_path: Path) -> None:
  backend = _build_backend(monkeypatch)

  backend._prepare_cwd(str(tmp_path))

  assert not (tmp_path / "AGENTS.md").exists()


def test_resume_session_id_fails_fast(monkeypatch) -> None:
  monkeypatch.setattr(
      "src.agents.backends.antigravity_cli.resolve_binary",
      lambda name, fallback: "/usr/bin/agy",
  )

  with pytest.raises(ValueError, match="does not support stable session resume"):
    AntigravityCliBackend(model="gemini-test-model", resume_session_id="session-123")


@pytest.mark.asyncio
async def test_run_emits_final_stdout_as_assistant_text(monkeypatch, tmp_path: Path) -> None:
  fake_agy = _write_fake_agy(
      tmp_path,
      """
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--print" ]; then
    shift
    break
  fi
  shift
done
printf 'final answer: %s\\n' "$1"
""",
  )
  monkeypatch.setattr(
      "src.agents.backends.antigravity_cli.resolve_binary",
      lambda name, fallback: str(fake_agy),
  )
  log_dir = tmp_path / "logs"
  backend = AntigravityCliBackend(model="gemini-test-model", log_dir=log_dir)

  events = await _consume(backend, tmp_path)

  assert events == [
      {
          "type": "assistant",
          "message": {
              "content": [{
                  "type": "text",
                  "text": "final answer: hello from CharlieBot",
              }]
          },
      },
      {
          "type": "result",
          "result": "",
          "usage":
              {
                  "input_tokens": 0,
                  "output_tokens": 0,
                  "cache_read_input_tokens": 0,
                  "cache_creation_input_tokens": 0,
              },
          "total_cost_usd": 0,
      },
  ]
  assert backend.exit_code == 0
  assert (log_dir / "stdout.log").read_text(encoding="utf-8") == "final answer: hello from CharlieBot\n"
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
  backend = AntigravityCliBackend(model="gemini-test-model")

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
  option = BackendOption(id="agy-gemini", label="Antigravity", type="antigravity", model="gemini-test-model")
  backend = build_backend(option, CharlieBotConfig(), extra_flags=["--sandbox"])

  assert isinstance(backend, AntigravityCliBackend)
  assert backend._build_command("hi") == [
      "/usr/bin/agy",
      "--print",
      "hi",
      "--print-timeout",
      "24h",
      "--dangerously-skip-permissions",
      "--sandbox",
  ]
