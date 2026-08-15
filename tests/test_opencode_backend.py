import json
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from src.agents.backends.opencode import OpenCodeBackend
from src.core import event_types as ET


def _build_backend(monkeypatch, **kwargs) -> OpenCodeBackend:
  monkeypatch.setattr(
      "src.agents.backends.opencode.resolve_binary",
      lambda name, fallback: "/usr/bin/opencode",
  )
  return OpenCodeBackend(**kwargs)


class _FakeOneShotStdout:

  def __aiter__(self):
    return self

  async def __anext__(self):
    raise StopAsyncIteration


def test_prepare_env_sets_charliebot_opencode_config(monkeypatch) -> None:
  backend = _build_backend(monkeypatch)

  env = backend._prepare_env({"PATH": "/usr/bin"})

  data = json.loads(env["OPENCODE_CONFIG_CONTENT"])
  assert data["permission"] == {"*": "allow", "question": "deny"}
  assert data["default_agent"] == "charliebot"
  assert data["agent"]["charliebot"] == {"mode": "primary"}


def test_prepare_env_merges_proxy_and_local_no_proxy_without_mutating_input(monkeypatch) -> None:
  backend = _build_backend(monkeypatch, opencode_proxy_url="http://proxy.test:8080")
  input_env = {
      "PATH": "/usr/bin",
      "NO_PROXY": "internal.test,localhost,127.0.0.1",
  }
  original_env = dict(input_env)

  prepared = backend._prepare_env(input_env)

  assert prepared["HTTP_PROXY"] == "http://proxy.test:8080"
  assert prepared["HTTPS_PROXY"] == "http://proxy.test:8080"
  assert prepared["NO_PROXY"] == "internal.test,localhost,127.0.0.1,::1"
  assert input_env == original_env

  repeated = backend._prepare_env(prepared)
  assert repeated["NO_PROXY"] == prepared["NO_PROXY"]


def test_prepare_env_without_proxy_preserves_proxy_related_environment(monkeypatch) -> None:
  backend = _build_backend(monkeypatch)
  input_env = {
      "PATH": "/usr/bin",
      "HTTP_PROXY": "http://existing-http.test:8080",
      "HTTPS_PROXY": "http://existing-https.test:8080",
      "NO_PROXY": "internal.test,localhost",
  }
  original_env = dict(input_env)

  prepared = backend._prepare_env(input_env)

  assert {
      key: prepared[key] for key in ("HTTP_PROXY", "HTTPS_PROXY", "NO_PROXY")
  } == {
      "HTTP_PROXY": "http://existing-http.test:8080",
      "HTTPS_PROXY": "http://existing-https.test:8080",
      "NO_PROXY": "internal.test,localhost",
  }
  assert input_env == original_env


def test_proxy_state_is_isolated_between_backend_instances(monkeypatch) -> None:
  proxied = _build_backend(monkeypatch, opencode_proxy_url="http://proxy.test:8080")
  unproxied = _build_backend(monkeypatch)

  proxied_env = proxied._prepare_env({"PATH": "/usr/bin"})
  unproxied_env = unproxied._prepare_env({"PATH": "/usr/bin"})

  assert proxied_env["HTTP_PROXY"] == "http://proxy.test:8080"
  assert proxied_env["HTTPS_PROXY"] == "http://proxy.test:8080"
  assert proxied_env["NO_PROXY"] == "localhost,127.0.0.1,::1"
  assert "HTTP_PROXY" not in unproxied_env
  assert "HTTPS_PROXY" not in unproxied_env
  assert "NO_PROXY" not in unproxied_env


class _SpawnObserved(Exception):
  """Halts the inherited base.run() exactly at the on_spawn notification."""


@pytest.mark.asyncio
async def test_run_passes_proxy_environment_and_config_to_run_subprocess(monkeypatch, tmp_path: Path) -> None:
  backend = _build_backend(monkeypatch, model="provider/model", opencode_proxy_url="http://proxy.test:8080")
  process = MagicMock()
  process.pid = 1234
  process.stdin = MagicMock()
  process.stdin.drain = AsyncMock()
  process.stdin.wait_closed = AsyncMock()
  create_process = AsyncMock(return_value=process)
  monkeypatch.setattr("src.agents.backends.base.asyncio.create_subprocess_exec", create_process)
  monkeypatch.setattr("src.core.runs.read_pid_stat", lambda pid: ("1", "R"))

  async def on_spawn(pid: int) -> None:
    raise _SpawnObserved

  backend._on_spawn = on_spawn
  input_env = {"PATH": "/usr/bin", "NO_PROXY": "internal.test,localhost"}
  original_env = dict(input_env)

  with pytest.raises(_SpawnObserved):
    async for _event in backend.run("prompt", str(tmp_path), input_env):
      pass

  await_args = create_process.await_args
  assert await_args.args[:6] == (
      "/usr/bin/opencode", "run", "--format", "json", "-m", "provider/model")
  child_env = await_args.kwargs["env"]
  assert child_env["HTTP_PROXY"] == "http://proxy.test:8080"
  assert child_env["HTTPS_PROXY"] == "http://proxy.test:8080"
  assert child_env["NO_PROXY"] == "internal.test,localhost,127.0.0.1,::1"
  assert json.loads(child_env["OPENCODE_CONFIG_CONTENT"])["permission"] == {
      "*": "allow",
      "question": "deny",
  }
  assert input_env == original_env


@pytest.mark.asyncio
async def test_one_shot_text_passes_proxy_environment_and_deny_policy(monkeypatch) -> None:
  backend = _build_backend(monkeypatch, model="provider/model", opencode_proxy_url="http://proxy.test:8080")
  monkeypatch.setenv("NO_PROXY", "internal.test,localhost")
  monkeypatch.setenv("HTTP_PROXY", "http://ambient-http.test:8080")
  monkeypatch.setenv("HTTPS_PROXY", "http://ambient-https.test:8080")
  process = MagicMock()
  process.stdout = _FakeOneShotStdout()
  process.stderr = MagicMock()
  process.stderr.read = AsyncMock(return_value=b"")
  process.wait = AsyncMock(return_value=0)
  process.pid = 5678
  process.returncode = 0
  create_process = AsyncMock(return_value=process)

  with patch("src.agents.backends.opencode.asyncio.create_subprocess_exec", new=create_process):
    result = await backend.one_shot_text("prompt", "system", timeout=5.0)

  child_env = create_process.await_args.kwargs["env"]
  assert result == ""
  assert child_env["HTTP_PROXY"] == "http://proxy.test:8080"
  assert child_env["HTTPS_PROXY"] == "http://proxy.test:8080"
  assert child_env["NO_PROXY"] == "internal.test,localhost,127.0.0.1,::1"
  assert json.loads(child_env["OPENCODE_CONFIG_CONTENT"]) == {"permission": {"*": "deny"}}
  process.wait.assert_awaited_once()


def test_prepare_cwd_writes_agents_md_when_instructions_provided(monkeypatch, tmp_path: Path) -> None:
  backend = _build_backend(monkeypatch, instructions_content="# Test Instructions\nDo things.")

  backend._prepare_cwd(str(tmp_path))

  agents_md = tmp_path / "AGENTS.md"
  assert agents_md.exists()
  assert agents_md.read_text(encoding="utf-8") == "# Test Instructions\nDo things."


def test_prepare_cwd_writes_agents_md_even_when_config_exists(monkeypatch, tmp_path: Path) -> None:
  """AGENTS.md must be written even when opencode.json already exists (resumed sessions)."""
  backend = _build_backend(monkeypatch, instructions_content="# Instructions")
  config_dir = tmp_path / ".opencode"
  config_dir.mkdir()
  (config_dir / "opencode.json").write_text("{}", encoding="utf-8")

  backend._prepare_cwd(str(tmp_path))

  agents_md = tmp_path / "AGENTS.md"
  assert agents_md.exists()
  assert agents_md.read_text(encoding="utf-8") == "# Instructions"


def test_prepare_cwd_skips_agents_md_when_no_instructions(monkeypatch, tmp_path: Path) -> None:
  backend = _build_backend(monkeypatch)

  backend._prepare_cwd(str(tmp_path))

  agents_md = tmp_path / "AGENTS.md"
  assert not agents_md.exists()


def test_translate_tool_error_emits_tool_result(monkeypatch) -> None:
  backend = _build_backend(monkeypatch)

  translated = backend.translate_event({
      "type": "tool",
      "part": {
          "callID": "call-1",
          "tool": "glob",
          "state": {
              "input": {"pattern": "AGENTS.md", "path": "/tmp"},
              "error": "The user rejected permission to use this specific tool call.",
          },
      },
  })

  assert translated == [
      {
          "type": "assistant",
          "message": {
              "content": [{
                  "type": "tool_use",
                  "name": "glob",
                  "id": "call-1",
                  "input": {"pattern": "AGENTS.md", "path": "/tmp"},
              }]
          },
      },
      {
          "type": ET.TOOL_RESULT,
          "tool_use_id": "call-1",
          "content": "The user rejected permission to use this specific tool call.",
      },
  ]
