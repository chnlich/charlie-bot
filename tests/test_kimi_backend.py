import pytest

from src.agents.backends.kimi import KimiBackend


def test_prepare_env_sets_moonshot_endpoint_and_model() -> None:
  backend = KimiBackend(api_key="moonshot-test-key", model="kimi-k2.5")

  prepared = backend._prepare_env({"PATH": "/usr/bin"})

  assert prepared["PATH"] == "/usr/bin"
  assert prepared["CLAUDE_CODE_DISABLE_BACKGROUND_TASKS"] == "1"
  assert prepared["ANTHROPIC_BASE_URL"] == "https://api.moonshot.cn/anthropic"
  assert prepared["ANTHROPIC_AUTH_TOKEN"] == "moonshot-test-key"
  assert prepared["ANTHROPIC_MODEL"] == "kimi-k2.5"
  assert prepared["ANTHROPIC_DEFAULT_OPUS_MODEL"] == "kimi-k2.5"
  assert prepared["ANTHROPIC_DEFAULT_SONNET_MODEL"] == "kimi-k2.5"
  assert prepared["ANTHROPIC_DEFAULT_HAIKU_MODEL"] == "kimi-k2.5"
  assert prepared["CLAUDE_CODE_SUBAGENT_MODEL"] == "kimi-k2.5"


def test_build_command_does_not_pass_model_flag() -> None:
  backend = KimiBackend(api_key="moonshot-test-key", model="kimi-k2.5")

  cmd = backend._build_command("hello")

  assert "--model" not in cmd
  assert "hello" not in cmd
  assert backend._stdin_prompt("hello") == "hello"


def test_prepare_env_values_are_subprocess_safe() -> None:
  backend = KimiBackend(api_key="moonshot-test-key", model="kimi-k2.5")

  prepared = backend._prepare_env({"PATH": "/usr/bin"})

  assert all(isinstance(value, (str, bytes)) for value in prepared.values())


def test_requires_model() -> None:
  with pytest.raises(ValueError, match="requires a model"):
    KimiBackend(api_key="moonshot-test-key", model="")
